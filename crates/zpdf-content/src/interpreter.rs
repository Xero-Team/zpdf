use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use zpdf_core::{Matrix, ObjectId, ParseLimits, PdfDict, PdfObject, Point, Rect};
use zpdf_display_list::*;
use zpdf_document::page::ResourceDict;
use zpdf_font::FontCache;
use zpdf_image::ImageCache;
use zpdf_parser::PdfFile;

use crate::stats::InterpretStats;
use crate::tables::RuleLine;
use crate::text::{TextGlyph, TextSpan};
use crate::tokenizer::{BorrowedContentToken as ContentToken, ContentTokenizer};

/// Cached CMYK→RGB conversion entry
type CmykCacheEntry = ([f64; 4], (f64, f64, f64));

/// Result of expanding a stroke in user space under an anisotropic CTM.
enum AnisotropicStroke {
    Outline(Path),
    Empty,
    Fallback,
}

/// Interprets a PDF content stream and produces a DisplayList.
pub struct ContentInterpreter<'a> {
    state_stack: Vec<GraphicsState>,
    current: GraphicsState,
    display_list: DisplayList,
    current_path: Path,
    operand_stack: Vec<PdfObject>,
    text_active: bool,
    text_matrix: Matrix,
    text_line_matrix: Matrix,
    /// Page-space accumulator for text clipping (render modes 4–7): every glyph
    /// shown in the current BT…ET block appends its outline here, and `ET`
    /// intersects the union into the clip stack (see `apply_text_clip`). `None` =
    /// no glyph outlines accumulated yet. `text_clip_requested` records that a
    /// clip render mode was seen at all, even if no drawable glyph produced one.
    /// `text_clip_unavailable` records that some clipped glyphs had no outline
    /// source at all (Type3 CharProcs, no loaded font program), as opposed to
    /// glyphs whose outline is known to be empty (spaces).
    text_clip_accum: Option<Path>,
    text_clip_requested: bool,
    /// The page-space path and rule of a `W`/`W*` awaiting its painting
    /// operator: the new clip applies only after that operator paints (PDF
    /// 32000-1 §8.5.4), so `re W S` strokes under the previous clip.
    pending_clip: Option<(Path, FillRule)>,
    text_clip_unavailable: bool,
    font_cache: Option<&'a mut FontCache>,
    /// Document whose shared font cache form and ExtGState fonts load through
    /// (see [`Self::with_shared_fonts`]).
    shared_font_doc: Option<&'a zpdf_document::PdfDocument>,
    current_font_id: Option<zpdf_font::FontId>,
    /// Font IDs whose embedded-program fallback diagnostics have already been
    /// merged into `self.stats.diagnostics` during this interpretation.
    reported_font_fallbacks: Vec<zpdf_font::FontId>,
    /// Font IDs whose embedded-program fallback failed to recover at least one
    /// visible glyph outline and already recorded `missing_glyph_outlines`.
    reported_font_outline_misses: Vec<zpdf_font::FontId>,
    /// Font IDs whose embedded-program fallback already recorded a
    /// `decode_failures` entry (either from load-time limits or undecodable text).
    reported_font_text_misses: Vec<zpdf_font::FontId>,
    /// Per-page cache classifying shown glyph IDs in embedded fonts as healthy
    /// vs corrupt/unreadable so healthy PDFs avoid repeated face parsing while
    /// partially corrupt subsets trigger per-glyph descriptor substitution.
    embedded_glyph_health: Vec<(zpdf_font::FontId, EmbeddedGlyphHealth)>,
    file: Option<&'a PdfFile>,
    resources: Option<&'a ResourceDict>,
    image_cache: Option<&'a mut ImageCache>,
    /// Skip all image decoding, including self-contained Type 3 inline bitmaps
    /// and shading rasters (used by extraction-only callers that discard the
    /// display list).
    skip_images: bool,
    /// Skip visual CharProc replay for Type 3 glyphs (used by extraction-only
    /// callers that discard the display list so repeated vector glyphs do not
    /// exhaust the page's shared operator budget).
    skip_type3_painting: bool,
    max_image_cache_bytes: u64,
    max_font_cache_bytes: u64,
    /// Active document/standalone parsing policy used by inline image filters
    /// and all image pixel allocations.
    parse_limits: ParseLimits,
    icc_cache: Option<&'a mut zpdf_color::IccCache>,
    /// Default DeviceCMYK colour-management transform from the document's active
    /// output intent (`/OutputIntents` → `/DestOutputProfile`, 4-channel only).
    /// When present, DeviceCMYK fills/strokes/text/images convert through this
    /// ICC profile instead of the Adobe SWOP polynomial. Owned `Arc` (not behind
    /// `&mut icc_cache`) so the read-only colour-conversion paths can consult it.
    /// `None` ⇒ every conversion keeps the SWOP polynomial (byte-identical to
    /// pre-output-intent behaviour).
    output_intent_cmyk: Option<std::sync::Arc<zpdf_color::IccTransform>>,
    /// Maps an image XObject's object id and stored size to its entry in
    /// `image_cache`, so an image drawn many times (e.g. a map symbol repeated
    /// thousands of times) is decoded — colour-converted, soft-mask-folded — only
    /// once instead of per `Do`. Without [`Self::with_image_downscale`] every
    /// draw stores the decoded size. Keyed only for stateless images;
    /// `/ImageMask` stencils bake in the current fill colour and are never
    /// cached here.
    image_obj_cache: HashMap<(ObjectId, u32, u32), zpdf_display_list::ImageId>,
    /// `/ImageMask` stencils keyed by object id, the fill colour baked into
    /// them and their stored size. A stencil repeated across a page (or a
    /// tiling-pattern cell) in one colour is decoded once instead of per `Do`.
    image_mask_cache: HashMap<(ObjectId, [u8; 3], u32, u32), zpdf_display_list::ImageId>,
    /// Decoded size of each stencil in `image_mask_cache`.
    image_mask_sizes: HashMap<ObjectId, (u32, u32)>,
    /// Decoded size of each image XObject in `image_obj_cache`, and its pixels
    /// while they are retained to filter further footprints.
    image_sources: HashMap<ObjectId, ImageSource>,
    /// Bytes of the sources retained in `image_sources`. They count against
    /// `max_image_cache_bytes` together with the image cache.
    retained_image_bytes: u64,
    /// CPU render scale for [`Self::with_image_downscale`]; `0.0` stores
    /// decoded images at their source size.
    image_downscale_scale: f32,
    /// Destination raster scale for [`Self::with_clip_elision_scale`]; `0.0`
    /// keeps all clip commands (scale-independent display list).
    clip_elision_scale: f32,
    rejected_image_objects: HashSet<ObjectId>,
    /// Built shadings (256-entry LUT + parsed functions) cached by object id, so a
    /// shading painted many times (`sh` markers repeated across a map) rebuilds its
    /// LUT only once. Reuse overwrites `to_page` with the current CTM.
    shading_cache: HashMap<ObjectId, crate::shading::ShadingDef>,
    /// Decoded form XObject content by object id (see
    /// [`MAX_CONTENT_CACHE_BYTES`]).
    form_content_cache: HashMap<ObjectId, std::sync::Arc<[u8]>>,
    /// Tiling patterns built at `scn` by object id, `None` for an unusable
    /// pattern, so reselecting a pattern does not decode its cell again.
    tiling_pattern_cache: HashMap<ObjectId, Option<std::sync::Arc<TilingPatternDef>>>,
    /// Pre-interpreted single-cell display lists for tiling patterns, keyed by
    /// `(pattern object id, uncolored scn tint, effective cell downscale,
    /// enclosing resource scopes)`. Multiple variants per key are stored only
    /// when the cell resolves an inherited pattern from an enclosing scope whose
    /// relative transform differs across placements; self-contained cells share
    /// a single `Arc<DisplayList>` across all placements.
    tiling_cell_cache: HashMap<TilingCellKey, Vec<CachedTilingCell>>,
    /// Total cached entries stored across all buckets of `tiling_cell_cache`.
    tiling_cell_cache_entries: usize,
    /// Per-active-tiling-cell stack tracking `(outer_form_resources_len, scopes)`
    /// for inherited pattern lookups so cached cells record which enclosing
    /// scope transforms they actually depend on.
    tiling_inherited_pattern_scopes: Vec<(usize, Vec<InheritedPatternScope>)>,
    /// Commands retained in detached display lists (currently active outer
    /// display lists plus unique pre-interpreted tiling cells) that count
    /// against `max_commands` alongside `self.display_list.commands.len()`.
    retained_detached_commands: usize,
    /// Set when any nested form XObject or tiling pattern hits `MAX_FORM_DEPTH`
    /// or a cycle guard; prevents a degraded cell built near the nesting ceiling
    /// from poisoning `tiling_cell_cache` for shallower draws.
    nesting_limit_hit: bool,
    /// Deepest `form_depth` reached during the current form/pattern subtree,
    /// used to compute each cached tiling cell's `subtree_depth`.
    max_depth_reached: u32,
    /// Shading patterns built at `scn`, by pattern object and the `base_ctm`
    /// bits they were built under (`None` for an unusable pattern). Content
    /// that reselects one pattern per path reuses one evaluated shading (and
    /// one mesh tessellation) rather than rebuilding and storing it per fill.
    shading_pattern_cache: HashMap<ShadingPatternKey, Option<ShadingPatternPaint>>,
    /// Bytes held by `form_content_cache`, `tiling_pattern_cache` and
    /// `shading_pattern_cache`.
    content_cache_bytes: usize,
    /// Soft masks built outside tiling patterns (which reuse masks across
    /// tiles separately), so content that repeats `/GS1 gs` per object builds
    /// the mask group once. See [`SoftMaskCacheKey`].
    soft_mask_cache: HashMap<SoftMaskCacheKey, Option<SoftMask>>,
    form_font_overrides: Vec<HashMap<String, String>>,
    /// Bounded recovery for malformed CAD text fonts (broken subsets, fonts
    /// leaked across text blocks), fed while the page is interpreted.
    font_repair: crate::font_repair::FontRepair,
    /// The current font is a leak override for this show operator, not one
    /// a `Tf` selected; its shows are not recovery evidence.
    font_override: bool,
    /// Owned `/Resources` of active Form XObjects, tiling patterns, and Type 3
    /// scopes, paired with each defining scope's default `base_ctm` (ISO 32000-1
    /// §8.7.2), innermost last. Lookups search these before the page resources.
    form_resources: Vec<(std::sync::Arc<ResourceDict>, Matrix)>,
    /// Stable identity for each pushed entry in `form_resources`, used to key
    /// `soft_mask_cache` and `tiling_cell_cache` so sibling scopes with
    /// distinct resource bindings never alias each other's inherited resources.
    form_resource_scopes: Vec<ResourceScopeId>,
    /// Monotonic counter for anonymous inline resource scopes.
    next_resource_scope_id: u64,
    form_depth: u32,
    /// Object ids of the form XObjects currently being interpreted. A form
    /// that draws itself (directly, or through a pattern cell) recurses
    /// forever, so re-entry is refused instead of fanning out to the budget.
    active_forms: Vec<ObjectId>,
    /// Object ids of tiling patterns whose cell is currently being interpreted,
    /// breaking self-referential pattern cycles immediately.
    active_tiling_patterns: Vec<ObjectId>,
    /// Active `(FontId, glyph_id)` pairs for Type 3 CharProcs currently being
    /// interpreted, guarding against direct or indirect self-recursion.
    active_type3_glyphs: Vec<(FontId, u16)>,
    /// Cached parsed `/Resources`, pre-loaded font bindings, and `/CharProcs`
    /// stream IDs for Type 3 fonts interpreted on this page (parsed once per
    /// Type 3 font within `max_type3_resource_cache_bytes`).
    type3_font_resources: HashMap<FontId, std::sync::Arc<CachedType3FontResources>>,
    /// Shared parsed `/Resources` scopes keyed by indirect `/Resources` object
    /// ID so distinct CharProc streams (or a Type 3 font and its CharProcs)
    /// referencing the same indirect resource dictionary reuse one `Arc`.
    type3_shared_resources: HashMap<ObjectId, std::sync::Arc<CachedType3ResourceScope>>,
    /// Cached per-CharProc `/Resources` lookup keyed by CharProc stream object
    /// ID (`None` when the CharProc carries no `/Resources`).
    type3_glyph_resources: HashMap<ObjectId, Option<std::sync::Arc<CachedType3ResourceScope>>>,
    /// Estimated bytes retained across `type3_font_resources`,
    /// `type3_shared_resources`, and `type3_glyph_resources`.
    type3_resource_cache_bytes: usize,
    /// Hard cap on retained Type 3 resource scopes for this page; scopes beyond
    /// the cap are still parsed for the current glyph and dropped afterward.
    max_type3_resource_cache_bytes: usize,
    /// Set when any painting operation inside the active Type 3 glyph is
    /// suppressed by an inactive optional-content group (`oc_suppressed()`).
    type3_oc_suppressed_mark: bool,
    /// Set when an image decode/admission drops an image inside the active
    /// Type 3 glyph so the incomplete glyph is excluded from `expanded_glyph_runs`.
    type3_image_dropped: bool,
    /// Set when an image draw inside the active Type 3 glyph carries paint
    /// outside the caller's solid fill (non-mask image or `d0`-recolored stencil).
    type3_local_image_paint: bool,
    /// Caller solid fill color active when entering the innermost Type 3 glyph.
    type3_caller_fill: Option<Color>,
    /// Active Type 3 glyph context `(font_id, glyph_id, inline_image_ordinal)`
    /// used to deduplicate repeated inline bitmap images (e.g. TeX Type 3
    /// glyphs) into a single `ImageId` per `(glyph, colour, footprint)`.
    type3_inline_ctx: Option<(FontId, u16, u32)>,
    /// Deduplication cache for inline images decoded inside Type 3 glyphs when
    /// an external `ImageCache` is supplied.
    #[allow(clippy::type_complexity)]
    type3_inline_image_cache: HashMap<
        (FontId, u16, u32, Type3InlineColorKey, (u32, u32)),
        (zpdf_display_list::ImageId, bool),
    >,
    /// Deduplication cache for Type 3 inline images embedded directly on
    /// `RenderCommand::DrawInlineImage` when no external `ImageCache` was
    /// supplied.
    #[allow(clippy::type_complexity)]
    type3_inline_fallback_cache: HashMap<
        (FontId, u16, u32, Type3InlineColorKey, (u32, u32)),
        std::sync::Arc<zpdf_display_list::InlineImage>,
    >,
    /// Deduplication cache for Type 3 Image XObjects embedded directly on
    /// `RenderCommand::DrawInlineImage` when no external `ImageCache` was
    /// supplied.
    #[allow(clippy::type_complexity)]
    type3_inline_xobject_cache: HashMap<
        (ObjectId, Option<[u8; 3]>, (u32, u32)),
        std::sync::Arc<zpdf_display_list::InlineImage>,
    >,
    /// Original decoded source dimensions of non-stencil Image XObjects seen
    /// on the no-`ImageCache` Type 3 inline path, so footprint lookup uses the
    /// source size rather than a previously downscaled cached copy.
    type3_inline_xobject_sizes: HashMap<ObjectId, (u32, u32)>,
    /// Footprints rejected for image budget on the no-`ImageCache` Type 3
    /// XObject path, scoped per `(obj_id, footprint)` so a smaller footprint
    /// of the same object can still be admitted if it fits the remaining budget.
    type3_rejected_xobject_footprints: HashSet<(ObjectId, (u32, u32))>,
    /// Decoded RGBA bytes retained by `DrawInlineImage` commands emitted
    /// without an external `ImageCache`, charged against `max_image_cache_bytes`.
    type3_inline_fallback_bytes: u64,
    /// `Q` must never pop the state stack below this index — a form XObject
    /// raises it past its own saved state so an unbalanced `Q` inside the
    /// form cannot corrupt the page-level state.
    state_floor: usize,
    text_sink: Option<&'a mut Vec<TextSpan>>,
    /// Parallel owning-stream bindings for spans pushed into `text_sink` when
    /// running in structured text mode.
    span_streams: Option<&'a mut Vec<crate::text::SpanStreamBinding>>,
    /// Whether collected spans also carry positioned glyph cells.
    text_geometry: bool,
    /// Selection cells held by the sink and the open replacement, across this
    /// page and its forms. Cells of a run that is not kept are not counted.
    text_glyphs_collected: usize,
    /// UTF-8 produced by text decoding and marked-content replacements.
    extracted_text_bytes: u64,
    /// Collect drawn ruled lines (thin axis-aligned strokes/rect fills) for
    /// table detection. Like `text_sink`, only meaningful during extraction —
    /// installing it never changes the emitted display list.
    rule_sink: Option<&'a mut Vec<RuleLine>>,
    /// The CTM at page-content start (identity or the baked page rotation).
    /// Pattern space is anchored to this, not to the CTM at fill time.
    /// Base CTM of the stream currently being interpreted (page default user
    /// space, or identity inside a tiling cell's pattern space).
    base_ctm: Matrix,
    /// Graphics state captured at entry to the enclosing stream (page default
    /// state at page start, or the invoking graphics state on Form XObject
    /// entry) so tiling pattern cells inherit it per ISO 32000-1 §8.7.3.1.
    base_state: GraphicsState,
    /// Active font ID captured alongside `base_state` at stream entry.
    base_font_id: Option<zpdf_font::FontId>,
    /// Cumulative transform from the current display list's coordinate space to
    /// page space (`Matrix::identity()` at page level, `to_page` inside a tiling
    /// pattern cell). Used to size shading rasters from their transformed
    /// page-space footprint while keeping their placement in cell coordinates.
    shading_space_to_page: Matrix,
    /// Snapshot of top-level page `base_ctm` preserved across nested Form,
    /// pattern, and Type 3 scopes so patterns resolved from page `/Resources`
    /// remain anchored to page default user space (ISO 32000-1 §8.7.2).
    page_base_ctm: Matrix,
    /// Inside an uncolored (PaintType 2) tiling-pattern cell or `d1` Type 3
    /// glyph: color operators are ignored so marks inherit the enclosing paint.
    suppress_color_ops: bool,
    /// True while interpreting a Type 3 glyph CharProc stream (ISO 32000-1
    /// §9.6.5): marks painted before an explicit `d0` color operator — and all
    /// marks inside a `d1` glyph — emit `Paint::Inherited` so backends apply
    /// the enclosing text paint while preserving explicit `d0` fill/stroke
    /// colors and stencils.
    type3_glyph: bool,
    /// Pre-resolved named resources from the Type 3 font's `/Resources` dict so
    /// CharProcs can invoke `/Name Do`, `/GS gs`, and named `/ColorSpace` entries
    /// during standalone replay without a live `PdfFile`.
    type3_resources: Option<&'a zpdf_font::Type3Resources>,
    /// Nested `/Resources` pushed while replaying Form XObjects inside
    /// `type3_resources`.
    type3_resource_stack: Vec<std::sync::Arc<zpdf_font::Type3Resources>>,
    /// Default optional-content configuration; `None` renders everything.
    oc_config: Option<&'a zpdf_document::OcConfig>,
    /// Annotations painted after the page content (appearance streams).
    annotations: Option<&'a [zpdf_document::Annotation]>,
    /// Marked-content nesting depth (BMC/BDC vs EMC).
    mc_depth: u32,
    compatibility_depth: u32,
    /// Stack of marked-content `/MCID`s, one entry per open marked-content
    /// sequence (`None` for a sequence with no `/MCID`), parallel to `mc_depth`.
    /// Lets the text sink tag each span with the marked-content id that binds it
    /// to a Tagged-PDF structure element. Only meaningful during text extraction.
    mc_stack: Vec<MarkedContent>,
    /// Only the outer replacement reaches the text sink. It also covers
    /// glyphs shown by forms, whose marked-content stacks are independent.
    active_actual_text: Option<ActualText>,
    /// Stream-scoped `(owning_stream, mcid)` pairs on this page subsumed by a
    /// structure element's `/ActualText` during structured text extraction
    /// (`None` = page content stream; `Some(id)` = Form XObject stream `/Stm`).
    /// Empty outside structured mode.
    struct_actual_text_mcids: std::collections::HashSet<(Option<ObjectId>, i64)>,
    /// Content stream that owns the current `mc_stack`: `Some(None)` on the
    /// page content stream, `Some(Some(id))` inside an indirect Form XObject
    /// `id`, and `None` inside inline forms or tiling patterns.
    current_mc_stream: Option<Option<ObjectId>>,
    /// True when the current Form XObject was invoked inside an enclosing
    /// marked-content sequence already covered by a structure element's
    /// `/ActualText` replacement.
    enclosing_struct_actual_text: bool,
    actual_text_bytes: usize,
    max_actual_text_bytes: usize,
    /// Depth at which a hidden `BDC /OC` block began; painting is suppressed
    /// until the matching EMC.
    oc_hidden_from: Option<u32>,
    /// Hard ceiling on emitted display-list commands. Adversarial or pathological
    /// content (huge form-XObject fanout, massive tiling-pattern replication,
    /// repeated shadings) can otherwise build a multi-million-command list that
    /// hangs the renderer. Once exceeded, the interpreter stops emitting and
    /// returns the partial page rather than hanging.
    max_commands: usize,
    /// Cumulative path points retained across all expanded anisotropic stroke
    /// outlines on this page (including detached tiling cells, soft masks, and
    /// forms). Bounded by `max_anisotropic_stroke_points` so repeated nonuniform
    /// dashed strokes fall back before retaining unbounded point buffers.
    anisotropic_stroke_points: usize,
    /// Hard ceiling on cumulative points produced by `expand_anisotropic_stroke`
    /// across the whole page (see [`DEFAULT_MAX_ANISOTROPIC_STROKE_POINTS`]).
    max_anisotropic_stroke_points: usize,
    /// Total operators executed across the page and all nested forms/patterns.
    /// A command-count ceiling alone misses *exponential form-XObject fanout*,
    /// where each call emits almost nothing yet spawns many more calls — the
    /// command list grows slowly while the call tree explodes. Counting executed
    /// operators bounds that work directly.
    ops_executed: u64,
    /// Ceiling for `ops_executed` (see [`DEFAULT_MAX_OPS`]).
    max_ops: u64,
    /// Wall-clock deadline for interpreting this page, the catch-all anti-hang
    /// backstop. Some heavy work (e.g. rasterizing a shading into an image per
    /// tiling-pattern cell) costs little in operators or emitted commands yet
    /// much in time, so the count budgets above cannot bound it. Sampled at the
    /// loop checkpoints. `None` disables it.
    deadline: Option<zpdf_core::time::Instant>,
    /// Latches once `deadline` passes, so the clock is read only occasionally.
    timed_out: bool,
    /// `ops_executed` at the last clock sample (throttles `Instant::now`).
    last_clock_op: u64,
    /// Cache last CMYK→RGB conversion to avoid redundant transforms.
    /// Technical drawings often paint hundreds of shapes in the same black/cyan color.
    /// Uses RefCell for interior mutability since cmyk_to_display is called from &self methods.
    cmyk_cache: RefCell<Option<CmykCacheEntry>>,
    /// Cache resolved color spaces per page (indirect refs can repeat).
    /// Cleared per-page to avoid stale references across documents.
    cs_cache: RefCell<HashMap<ObjectId, ActiveColorSpace>>,
    /// Maximum operand stack depth (M3 security fix).
    max_operand_stack_depth: usize,
    /// Maximum retained `q` graphics-state depth.
    max_graphics_state_depth: usize,
    /// Maximum BMC/BDC nesting depth.
    max_marked_content_depth: usize,
    /// Latches when a structural memory limit is exceeded so every enclosing
    /// form/pattern loop stops after restoring its local state.
    resource_limit_hit: bool,
    /// Interpret-stage timings; see [`InterpretStats`]. The whole-stage and
    /// shading clocks are always on (one pair per page, one per shading raster);
    /// the per-category work buckets inside it are opt-in — see
    /// [`Self::with_work_timing`].
    stats: InterpretStats,
    /// Collect the per-category work buckets in `stats`. Off by default: those
    /// clocks sit on every attributed operator, and a vector page can carry
    /// hundreds of thousands of them.
    work_timing: bool,
}

/// A cached image XObject's decoded source.
struct ImageSource {
    width: u32,
    height: u32,
    /// Decoded bytes of the source.
    bytes: u64,
    /// Rendering intent of the first decode, which a second decode reuses.
    intent: zpdf_color::RenderIntent,
    /// Times the stream has been decoded; never more than two.
    decodes: u8,
    /// Bytes of the object's filtered copies in the image cache. They never
    /// exceed `bytes`: a draw that would pass it uses the source instead.
    filtered_bytes: u64,
    /// The source is in the image cache; draws without a cached footprint
    /// use it, and the CPU backend filters it on draw.
    unfiltered: bool,
    /// The decoded pixels, kept from the second decode on so that further
    /// footprints need no decode. Counted against the image byte limit.
    retained: Option<zpdf_image::DecodedImage>,
}

#[derive(Clone, Default)]
struct MarkedContent {
    mcid: Option<i32>,
    ends_actual_text: bool,
}

struct ActualText {
    span: TextSpan,
    stream_binding: crate::text::SpanStreamBinding,
    has_source: bool,
    /// Keep admitted buffers until EMC so the final cluster allocates once.
    following_glyphs: Vec<Vec<TextGlyph>>,
}

impl ActualText {
    /// Add one show to the cluster. `retained` is the page's count of held
    /// glyph cells: it grows with the cells kept here and gets back the ones
    /// an approximate cluster gives up.
    fn add_source(&mut self, mut run: TextSpan, retained: &mut usize) {
        if !self.has_source {
            *retained += run.glyphs.len();
            run.text = std::mem::take(&mut self.span.text);
            run.mcid = self.span.mcid;
            self.span = run;
            self.has_source = true;
            return;
        }
        self.span.advance = run.x + run.advance - self.span.x;
        if run.glyphs.is_empty() {
            // One omitted run makes the whole cluster approximate.
            *retained = retained.saturating_sub(self.release_glyphs());
        } else if !self.span.glyphs.is_empty() {
            *retained += run.glyphs.len();
            self.following_glyphs.push(run.glyphs);
        }
    }

    /// Drop the cluster's cells and return how many it held.
    fn release_glyphs(&mut self) -> usize {
        let released =
            self.span.glyphs.len() + self.following_glyphs.iter().map(Vec::len).sum::<usize>();
        self.span.glyphs = Vec::new();
        self.following_glyphs.clear();
        released
    }

    /// Publish the replacement. A cluster that a page limit cut short keeps
    /// its text, like the spans before it, but only a complete cluster keeps
    /// its glyph cells.
    fn finish(
        mut self,
        complete: bool,
        sink: &mut Vec<TextSpan>,
        span_streams: Option<&mut Vec<crate::text::SpanStreamBinding>>,
        retained: &mut usize,
    ) {
        let publish = !self.span.text.is_empty() && self.span.is_finite();
        if !(publish && complete) {
            *retained = retained.saturating_sub(self.release_glyphs());
        }
        if !publish {
            return;
        }
        self.span
            .glyphs
            .reserve_exact(self.following_glyphs.iter().map(Vec::len).sum());
        for run in self.following_glyphs {
            self.span.glyphs.extend(run);
        }
        sink.push(self.span);
        if let Some(streams) = span_streams {
            streams.push(self.stream_binding);
        }
    }
}

/// Per-GID cached classification for an embedded font program. Exhausted
/// reverse-`cmap` probes are intentionally left uncached (`None`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CachedGlyphHealth {
    /// Confirmed readable (drawn outline or zero-length `loca` blank handled
    /// by CAD repair).
    Readable,
    /// Physically unreadable outline record (missing, malformed header, failed
    /// or empty visible Type 1 charstring, or out-of-range GID).
    Unreadable,
    /// Physically blank record whose readability depends on the shown PDF code
    /// (Type 1 blank with `/ToUnicode` or TrueType valid-header nonempty
    /// zero-contour record). When `fallback` is `Some(unreadable)`, a reverse
    /// `cmap` probe already determined whether the font's own `cmap` maps a
    /// visible scalar to this GID, so codes without direct PDF evidence reuse
    /// that fallback without re-probing while codes with direct `/ToUnicode`
    /// evidence (visible text or explicit whitespace) still win.
    CodeDependentBlank { fallback: Option<bool> },
}

/// Per-page cache of whether each shown glyph ID in an embedded font maps to a
/// corrupt/unreadable outline record. Inline 256-bit bitsets keep simple fonts
/// and small subsets allocation-free, and `cmap_budget` bounds blank-record
/// `cmap` validation across the whole page for this font.
struct EmbeddedGlyphHealth {
    known_low: [u64; 4],
    unreadable_low: [u64; 4],
    code_dep_low: [u64; 4],
    fallback_known_low: [u64; 4],
    high: HashMap<u16, CachedGlyphHealth>,
    cmap_budget: usize,
}

impl Default for EmbeddedGlyphHealth {
    fn default() -> Self {
        Self {
            known_low: [0; 4],
            unreadable_low: [0; 4],
            code_dep_low: [0; 4],
            fallback_known_low: [0; 4],
            high: HashMap::new(),
            cmap_budget: zpdf_font::CMAP_PROBE_BUDGET,
        }
    }
}

impl EmbeddedGlyphHealth {
    fn get(&self, gid: u16) -> Option<CachedGlyphHealth> {
        if gid < 256 {
            let word = (gid >> 6) as usize;
            let bit = 1u64 << (gid & 63);
            if (self.known_low[word] & bit) == 0 {
                return None;
            }
            if (self.code_dep_low[word] & bit) != 0 {
                let fallback = ((self.fallback_known_low[word] & bit) != 0)
                    .then_some((self.unreadable_low[word] & bit) != 0);
                Some(CachedGlyphHealth::CodeDependentBlank { fallback })
            } else if (self.unreadable_low[word] & bit) != 0 {
                Some(CachedGlyphHealth::Unreadable)
            } else {
                Some(CachedGlyphHealth::Readable)
            }
        } else {
            self.high.get(&gid).copied()
        }
    }

    fn set(&mut self, gid: u16, health: CachedGlyphHealth) {
        if gid < 256 {
            let word = (gid >> 6) as usize;
            let bit = 1u64 << (gid & 63);
            self.known_low[word] |= bit;
            match health {
                CachedGlyphHealth::Readable => {
                    self.unreadable_low[word] &= !bit;
                    self.code_dep_low[word] &= !bit;
                    self.fallback_known_low[word] &= !bit;
                }
                CachedGlyphHealth::Unreadable => {
                    self.unreadable_low[word] |= bit;
                    self.code_dep_low[word] &= !bit;
                    self.fallback_known_low[word] &= !bit;
                }
                CachedGlyphHealth::CodeDependentBlank { fallback } => {
                    self.code_dep_low[word] |= bit;
                    match fallback {
                        Some(unreadable) => {
                            self.fallback_known_low[word] |= bit;
                            if unreadable {
                                self.unreadable_low[word] |= bit;
                            } else {
                                self.unreadable_low[word] &= !bit;
                            }
                        }
                        None => {
                            self.fallback_known_low[word] &= !bit;
                            self.unreadable_low[word] &= !bit;
                        }
                    }
                }
            }
        } else if self.high.len() < 4096 || self.high.contains_key(&gid) {
            self.high.insert(gid, health);
        }
    }
}

/// Outcome of storing a draw of a known image XObject without decoding.
enum Stored {
    Image(zpdf_image::ImageId),
    /// Nothing can be stored without a third decode, or the cache is full.
    Skipped,
    /// No decoded pixels are available.
    NeedsDecode,
}

/// Bytes of an RGBA image of `size`.
fn rgba_bytes(size: (u32, u32)) -> u64 {
    size.0 as u64 * size.1 as u64 * 4
}

/// Stable identity of one pushed `/Resources` scope on `form_resources`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ResourceScopeId {
    /// Indirect `/Resources` dictionary object shared by one or more streams.
    ResourcesObject(ObjectId),
    /// Inline `/Resources` dictionary owned by an indirect Form XObject or
    /// tiling pattern stream.
    OwnerObject(ObjectId),
    /// Anonymous inline `/Resources` dictionary with no indirect owner; each
    /// push receives a distinct monotonic ID so sibling scopes never alias.
    Unique(u64),
}

/// Cacheable snapshot of the inherited graphics state that affects both
/// soft-mask `/G` forms and tiling pattern cells (stroke width/cap/join/miter/
/// dash, fill/stroke colours, overprint, colour space, rendering intent, and
/// text state). Returns `None` for exotic colour spaces or active patterns so
/// callers bypass caching rather than risk false aliasing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InheritedGraphicsStateKey {
    fill_color: [u32; 4],
    stroke_color: [u32; 4],
    line_width: u32,
    line_cap: u8,
    line_join: u8,
    miter_limit: u32,
    dash: Option<(Vec<u32>, u32)>,
    fill_cs: u8,
    stroke_cs: u8,
    rendering_intent: u8,
    fill_overprint: bool,
    stroke_overprint: bool,
    overprint_mode: u8,
    fill_colorant: Option<([u32; 4], bool)>,
    stroke_colorant: Option<([u32; 4], bool)>,
    font_id: Option<zpdf_font::FontId>,
    font_name: String,
    text_scalars: [u32; 6],
    render_mode: u8,
}

impl InheritedGraphicsStateKey {
    fn cs_tag(cs: &ActiveColorSpace) -> Option<u8> {
        match cs {
            ActiveColorSpace::DeviceGray => Some(0),
            ActiveColorSpace::DeviceRGB => Some(1),
            ActiveColorSpace::DeviceCMYK => Some(2),
            ActiveColorSpace::ICCBased(n) => Some(3u8.saturating_add(*n)),
            _ => None,
        }
    }

    fn intent_tag(ri: zpdf_color::RenderIntent) -> u8 {
        match ri {
            zpdf_color::RenderIntent::AbsoluteColorimetric => 0,
            zpdf_color::RenderIntent::RelativeColorimetric => 1,
            zpdf_color::RenderIntent::Saturation => 2,
            zpdf_color::RenderIntent::Perceptual => 3,
        }
    }

    fn from_state(state: &GraphicsState, font_id: Option<zpdf_font::FontId>) -> Option<Self> {
        if state.fill_pattern.is_some() || state.stroke_pattern.is_some() {
            return None;
        }
        let fill_cs = Self::cs_tag(&state.fill_cs)?;
        let stroke_cs = Self::cs_tag(&state.stroke_cs)?;
        let line_cap = match state.line_cap {
            LineCap::Butt => 0,
            LineCap::Round => 1,
            LineCap::Square => 2,
        };
        let line_join = match state.line_join {
            LineJoin::Miter => 0,
            LineJoin::Round => 1,
            LineJoin::Bevel => 2,
        };
        let dash = state.dash.as_ref().map(|d| {
            (
                d.array.iter().map(|v| v.to_bits()).collect(),
                d.phase.to_bits(),
            )
        });
        let colorant_bits =
            |c: Option<Colorant>| c.map(|col| (col.cmyk.map(f32::to_bits), col.cmyk_device));
        Some(Self {
            fill_color: [
                state.fill_color.r,
                state.fill_color.g,
                state.fill_color.b,
                state.fill_color.a,
            ]
            .map(f32::to_bits),
            stroke_color: [
                state.stroke_color.r,
                state.stroke_color.g,
                state.stroke_color.b,
                state.stroke_color.a,
            ]
            .map(f32::to_bits),
            line_width: state.line_width.to_bits(),
            line_cap,
            line_join,
            miter_limit: state.miter_limit.to_bits(),
            dash,
            fill_cs,
            stroke_cs,
            rendering_intent: Self::intent_tag(state.rendering_intent),
            fill_overprint: state.fill_overprint,
            stroke_overprint: state.stroke_overprint,
            overprint_mode: state.overprint_mode,
            fill_colorant: colorant_bits(state.fill_colorant),
            stroke_colorant: colorant_bits(state.stroke_colorant),
            font_id,
            font_name: state.font_name.clone(),
            text_scalars: [
                state.font_size,
                state.char_spacing,
                state.word_spacing,
                state.h_scaling,
                state.leading,
                state.rise,
            ]
            .map(f32::to_bits),
            render_mode: state.render_mode,
        })
    }
}

/// What a soft mask built at a `gs` depends on: the ExtGState object, and the
/// graphics state the mask group starts from — its CTM (and the pattern
/// anchor), the clip bounds its `/BBox` and shadings are cut to, the full
/// inherited graphics state (colours, line width/cap/join/miter/dash, text,
/// overprint, colour space), and the stack of enclosing resource scopes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SoftMaskCacheKey {
    gs_id: ObjectId,
    matrices: [u64; 12],
    clip: Option<[u64; 4]>,
    inherited: InheritedGraphicsStateKey,
    resource_scopes: Vec<ResourceScopeId>,
    embed_type3_inline_images: bool,
}

/// Soft masks [`ContentInterpreter`] keeps per page; later masks are built
/// for every use.
const MAX_CACHED_SOFT_MASKS: usize = 256;

/// Cache key for pre-interpreted tiling-pattern cells. Coloured (`/PaintType 1`)
/// cells depend on the pattern object, effective cell downscale, page-space
/// shading scale, parent-stream entry state, enclosing resource scopes (since
/// `lookup_res` falls back through enclosing forms), and whether no-cache Type 3
/// inline-image embedding is active; uncoloured (`/PaintType 2`) cells also bake
/// in the `scn`/`SCN` paint colour.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TilingCellKey {
    pat_id: ObjectId,
    uncolored_color: Option<[u32; 4]>,
    image_downscale: u32,
    /// Page-space axis scales of the cell's transform, so enlarged and shrunk
    /// instances of the same pattern never alias each other's shading rasters.
    shading_scale: [u32; 2],
    inherited: InheritedGraphicsStateKey,
    resource_scopes: Vec<ResourceScopeId>,
    /// True when `image_cache` is absent and we are inside an active Type 3
    /// glyph, where inline/XObject/shading images inside the cell embed as
    /// `DrawInlineImage` rather than being omitted as on ordinary page fills.
    embed_type3_inline_images: bool,
}

/// Which enclosing resource scope supplied a pattern resolved inside a tiling
/// pattern cell (so `tiling_cell_cache` can verify relative scope transforms
/// only for scopes the cell actually inherited from).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum InheritedPatternScope {
    /// Page-level `/Resources`.
    Page,
    /// Index into the outer `form_resources` stack active when entering the cell.
    EnclosingForm(usize),
}

/// Cached tiling-pattern cell along with the nesting depth its subtree consumed
/// (at least 1 for the cell itself) and the exact cell-space transforms of any
/// enclosing scopes whose patterns were inherited by the cell body.
struct CachedTilingCell {
    commands: std::sync::Arc<DisplayList>,
    subtree_depth: u32,
    inherited_pattern_transforms: Vec<(InheritedPatternScope, [u64; 6])>,
}

/// Pre-interpreted tiling-pattern cells [`ContentInterpreter`] keeps per page.
const MAX_CACHED_TILING_CELLS: usize = 256;
/// Maximum distinct inherited-transform variants cached under one `TilingCellKey`.
const MAX_CACHED_TILING_VARIANTS_PER_KEY: usize = 16;

/// Wall-clock budget for interpreting a single page (see `deadline`). Generous
/// for any real page (which interpret in well under a second); bounds
/// adversarial shading/pattern blowups.
const INTERPRET_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// Page-wide allowance for optional selection cells. A tighter decoded-stream
/// budget lowers it as well. Text and rendering survive an omitted glyph run.
const MAX_TEXT_GEOMETRY_BYTES: u64 = 16 << 20;

/// One text operator can contain many glyphs or TJ elements, so its loops check
/// the deadline every 256 items. Index 0 is left out: an ordinary short show
/// never reads the clock and relies on the operator loop's sampling.
fn text_deadline_reached(index: usize, deadline: Option<zpdf_core::time::Instant>) -> bool {
    index != 0
        && index.is_multiple_of(256)
        && deadline.is_some_and(|end| zpdf_core::time::Instant::now() >= end)
}

/// Default [`ContentInterpreter::max_commands`]. Real pages run to at most low
/// hundreds of thousands of commands, so this is a generous headroom while still
/// cutting off runaway form-XObject/tiling/shading emitters before their
/// emission *time* (not just memory) becomes a hang. Tuned together with the
/// renderer's wall-clock budget so interpret + render stay comfortably bounded.
const DEFAULT_MAX_COMMANDS: usize = 500_000;

/// Default [`ContentInterpreter::max_anisotropic_stroke_points`]: 2M points
/// (~32 MiB) across all expanded anisotropic stroke outlines retained on one
/// page (including detached pattern cells and soft masks).
const DEFAULT_MAX_ANISOTROPIC_STROKE_POINTS: usize = 2_000_000;

/// Default [`ContentInterpreter::max_ops`]. Bounds total interpreter work
/// (operators executed) so exponential form/pattern fanout that emits few
/// commands per call still terminates promptly.
const DEFAULT_MAX_OPS: u64 = 4_000_000;

/// Decoded form XObject and tiling-pattern cell content kept per page, so a
/// form or pattern replayed many times (a symbol repeated across a map, a
/// form drawn in every tile) is inflated once. Past this many bytes further
/// content is decoded per use again.
const MAX_CONTENT_CACHE_BYTES: usize = 32 << 20;

/// Maximum retained bytes for cached Type 3 font/CharProc `/Resources` scopes
/// per page; distinct scopes past this cap are parsed per use and dropped.
const MAX_TYPE3_RESOURCE_CACHE_BYTES: usize = 4 << 20;

#[derive(Debug, Clone)]
enum ActiveColorSpace {
    DeviceGray,
    DeviceRGB,
    DeviceCMYK,
    CalGray(zpdf_color::CalGrayParams),
    CalRGB(zpdf_color::CalRGBParams),
    ICCBased(u8),
    /// ICCBased with a compiled profile→sRGB transform (shared through the
    /// document's `IccCache`). Without a usable profile the space resolves to
    /// the `/N`-matched device space (or `ICCBased(n)` for odd `/N`) instead.
    Icc(std::sync::Arc<zpdf_color::IccTransform>),
    Lab {
        white_point: [f64; 3],
        range: [f64; 4],
    },
    Indexed {
        base: Box<ActiveColorSpace>,
        hival: u8,
        lookup: std::sync::Arc<[u8]>,
    },
    /// Separation (n = 1) or DeviceN/NChannel (n = components) with its tint
    /// transform and alternate space. A missing/unparseable transform falls back
    /// to `gray(1 - max(tint))`, which has the right polarity for colorants.
    Tint {
        n: usize,
        transform: Option<std::sync::Arc<zpdf_color::PdfFunction>>,
        alternate: Box<ActiveColorSpace>,
        /// Colorant names (Separation: one; DeviceN/NChannel: `n`). Drives the
        /// special names `None` (no marks, ISO 32000-1 §8.6.6.4/§8.6.6.5) and
        /// `All` (Separation → every colorant, i.e. overprint knockout).
        names: std::sync::Arc<[String]>,
        /// PDF 2.0 NChannel `/Colorants`: per-input-colorant mapping to process
        /// colorants, parallel to `names`. Present only when **every** colorant
        /// is classifiable (a `/None`, a named process colorant, or a spot whose
        /// own Separation space is given in `/Colorants`); enables a precise
        /// per-colorant overprint mask. `None` → overprint uses the whole-
        /// transform projection (plain Separation/DeviceN, no regression).
        colorants: Option<std::sync::Arc<[ColorantMap]>>,
    },
    Pattern {
        /// Underlying space of `[/Pattern base]` — the color operands that
        /// accompany an uncolored (PaintType 2) tiling pattern in `scn`.
        base: Option<Box<ActiveColorSpace>>,
    },
}

impl ActiveColorSpace {
    fn components(&self) -> usize {
        match self {
            Self::DeviceGray | Self::CalGray(_) => 1,
            Self::DeviceRGB | Self::CalRGB(_) => 3,
            Self::DeviceCMYK => 4,
            Self::ICCBased(n) => (*n).max(1) as usize,
            Self::Icc(t) => t.components(),
            Self::Lab { .. } => 3,
            Self::Indexed { .. } => 1,
            Self::Tint { n, .. } => (*n).max(1),
            Self::Pattern { .. } => 0,
        }
    }
}

/// How one input colorant of a DeviceN/NChannel space maps to process colorants
/// for the overprint active-colorant mask (PDF 8.6.7). Built from `/Colorants`
/// (and standard process colorant names) so each colorant can be projected
/// individually — letting a `/None` colorant contribute no ink, which the
/// whole-transform projection cannot express.
#[derive(Debug, Clone)]
enum ColorantMap {
    /// `/None` — produces no marks; contributes nothing to the mask.
    None,
    /// A standard process colorant; index into `(C, M, Y, K)`.
    Process(usize),
    /// A spot colorant carrying its own Separation space (from `/Colorants`);
    /// the tint is projected through it to recover process colorants.
    Spot(Box<ActiveColorSpace>),
    /// An unclassifiable spot (no `/Colorants` entry, not a process name) that
    /// must still be projected because a sibling `/None` has to be dropped: run
    /// the DeviceN's own tint transform with only this component's tint set (the
    /// rest zeroed) and recover the alternate space's colorants. Less precise
    /// than a real per-colorant Separation, but it isolates the colorant so the
    /// `/None` component contributes no ink (ISO 32000-1 §8.6.6.5).
    Whole,
}

/// Index of a standard process colorant name into `(C, M, Y, K)`, or `None` for
/// a spot/other name. Used to classify DeviceN/NChannel colorants (§8.6.6.5).
fn process_colorant_index(name: &str) -> Option<usize> {
    match name {
        "Cyan" => Some(0),
        "Magenta" => Some(1),
        "Yellow" => Some(2),
        "Black" => Some(3),
        _ => None,
    }
}

/// True when `cs` is a Separation/DeviceN/NChannel space whose colorants are
/// **all** the special name `None`: such a space produces no marks on the page
/// (ISO 32000-1 §8.6.6.4 Separation, §8.6.6.5 DeviceN — "marking … in which all
/// colorant names are None shall produce no marks"), so paints in it are
/// suppressed entirely while text extraction is unaffected.
fn cs_produces_no_marks(cs: &ActiveColorSpace) -> bool {
    matches!(
        cs,
        ActiveColorSpace::Tint { names, .. }
            if !names.is_empty() && names.iter().all(|n| n == "None")
    )
}

/// Resolved pattern paint selected via `scn` in a Pattern colorspace.
#[derive(Debug, Clone)]
enum PatternPaint {
    Shading(std::sync::Arc<crate::shading::ShadingDef>),
    Tiling {
        def: std::sync::Arc<TilingPatternDef>,
        /// Initial coordinate space (`base_ctm`) of the stream in which `scn`
        /// selected this pattern (ISO 32000-1 §8.7.2).
        base_ctm: Matrix,
    },
}

/// A tiling pattern (PatternType 1), resolved at `scn` time and replicated
/// cell-by-cell at fill time.
#[derive(Debug)]
struct TilingPatternDef {
    /// Indirect object ID of the pattern stream (for caching, cycle checks, and
    /// reloading inline Type 3 fonts declared in the pattern's `/Resources`).
    pat_id: ObjectId,
    /// Decoded cell content stream.
    content: Vec<u8>,
    /// The pattern's raw stream dict (fonts/resources are loaded at paint time).
    dict: zpdf_core::PdfDict,
    bbox: Rect,
    x_step: f64,
    y_step: f64,
    /// Pattern space → default page user space.
    matrix: Matrix,
    /// 1 = colored, 2 = uncolored (cell ignores color operators and paints
    /// with the color given alongside the pattern name in `scn`).
    paint_type: i64,
    /// 1 = constant spacing, 2 = no distortion, 3 = constant spacing and faster tiling.
    tiling_type: u8,
}

/// The CMYK colorant projection of the current fill/stroke colour, kept so the
/// overprint emitter (PDF 8.6.7) can tell which device colorants a paint writes.
/// `None` for additive / CIE / Pattern spaces, where overprint is a no-op.
#[derive(Debug, Clone, Copy)]
struct Colorant {
    /// Source colour as process-colorant tints `(C, M, Y, K)`.
    cmyk: [f32; 4],
    /// True for DeviceCMYK / ICCBased(4): `/OPM` governs whether a 0 component
    /// is painted (mode 0, knockout) or left (mode 1, nonzero overprint). For
    /// DeviceGray and Separation/DeviceN the nonzero rule always applies.
    cmyk_device: bool,
}

/// Clamp a colorant tint to `0..=1`, mapping non-finite (NaN/Inf, which a
/// malformed tint transform can yield) to 0 so overprint descriptors stay
/// finite and the active-bit test (`!= 0.0`) is well-defined on both backends.
fn fin(v: f64) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}

/// DeviceGray `g` projected onto the black colorant: `(0,0,0,1−g)`.
fn gray_colorant(gray: f32) -> Colorant {
    Colorant {
        cmyk: [0.0, 0.0, 0.0, fin(1.0 - gray as f64)],
        cmyk_device: false,
    }
}

/// DeviceCMYK colorants verbatim (OPM-governed).
fn cmyk_colorant(c: f64, m: f64, y: f64, k: f64) -> Colorant {
    Colorant {
        cmyk: [fin(c), fin(m), fin(y), fin(k)],
        cmyk_device: true,
    }
}

/// Active clip mask extent in destination device pixels (`[x0, x1) × [y0, y1)`).
///
/// Tracked separately from [`GraphicsState::clip_bounds`] so clip elision never
/// perturbs `sh` shading sampling grids and always checks coverage of every
/// affected device pixel (including antialiased edges and fractional rasters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeviceClipBounds {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

#[derive(Debug, Clone, Copy)]
enum InlineFontOwner {
    /// Indirect container object (Form XObject, tiling pattern, Type 3 font, or
    /// CharProc stream) whose `/Resources /Font /<name>` holds the inline font.
    Container(ObjectId),
    /// Indirect `/Resources` dictionary whose `/Font /<name>` holds the inline
    /// font directly.
    ResourcesDict(ObjectId),
}

fn is_paint_command(cmd: &RenderCommand) -> bool {
    matches!(
        cmd,
        RenderCommand::FillPath { .. }
            | RenderCommand::StrokePath { .. }
            | RenderCommand::DrawGlyphRun(_)
            | RenderCommand::DrawImage(_)
            | RenderCommand::DrawInlineImage(_)
    )
}

fn colors_close(a: Color, b: Color) -> bool {
    const EPS: f32 = 1e-3;
    (a.r - b.r).abs() <= EPS
        && (a.g - b.g).abs() <= EPS
        && (a.b - b.b).abs() <= EPS
        && (a.a - b.a).abs() <= EPS
}

fn overprint_matches(a: Option<Overprint>, b: Option<Overprint>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.active == y.active
                && x.cmyk
                    .iter()
                    .zip(y.cmyk.iter())
                    .all(|(cx, cy)| (cx - cy).abs() <= 1e-3)
        }
        _ => false,
    }
}

fn command_matches_caller_fill(
    cmd: &RenderCommand,
    caller_color: Color,
    caller_alpha: f32,
    caller_overprint: Option<Overprint>,
) -> bool {
    const EPS: f32 = 1e-3;
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
            matches!(*paint, Paint::Solid(c) if colors_close(c, caller_color) && (*alpha - caller_alpha).abs() <= EPS)
                && overprint_matches(*overprint, caller_overprint)
        }
        RenderCommand::DrawGlyphRun(run) => {
            matches!(run.paint, Paint::Solid(c) if colors_close(c, caller_color) && (run.alpha - caller_alpha).abs() <= EPS)
                && overprint_matches(run.overprint, caller_overprint)
        }
        RenderCommand::DrawImage(d) => {
            d.is_image_mask && (d.alpha - caller_alpha).abs() <= EPS && caller_overprint.is_none()
        }
        RenderCommand::DrawInlineImage(d) => {
            d.image.is_image_mask
                && (d.alpha - caller_alpha).abs() <= EPS
                && caller_overprint.is_none()
        }
        RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => false,
        RenderCommand::PopClip | RenderCommand::PopBlendGroup => true,
        RenderCommand::PushBlendGroup {
            blend_mode,
            alpha,
            mask,
            ..
        } => mask.is_none() && *blend_mode == BlendMode::Normal && (*alpha - 1.0).abs() <= EPS,
    }
}

#[derive(Debug, Clone)]
enum CachedFontBinding {
    /// Font resource points to the same `ObjectId` as the page's font under
    /// this name; only pushed as an explicit rebinding when an enclosing scope
    /// shadowed the name.
    PageSlot(String),
    /// Font resource maps `resource_name` to an already-admitted cache slot.
    CacheSlot(String, String),
}

#[derive(Debug)]
struct CachedType3ResourceScope {
    scope_id: ResourceScopeId,
    parsed_resources: Option<std::sync::Arc<ResourceDict>>,
    font_bindings: Vec<CachedFontBinding>,
}

#[derive(Debug, Default)]
struct CachedType3FontResources {
    scope: Option<std::sync::Arc<CachedType3ResourceScope>>,
    char_proc_ids: HashMap<String, ObjectId>,
}

/// Iteratively estimate retained heap bytes for an inline PDF object tree inside
/// a cached `ResourceDict` (no recursion, so deep dicts cannot overflow stack).
fn estimate_pdf_object_bytes_iter(root: &PdfObject) -> usize {
    enum Node<'a> {
        Dict(&'a PdfDict),
        Obj(&'a PdfObject),
    }

    let mut total = 0usize;
    let mut stack = vec![Node::Obj(root)];
    while let Some(node) = stack.pop() {
        match node {
            Node::Dict(d) => {
                total = total.saturating_add(64);
                for (k, v) in &d.0 {
                    total = total.saturating_add(32).saturating_add(k.0.len());
                    stack.push(Node::Obj(v));
                }
            }
            Node::Obj(obj) => match obj {
                PdfObject::Null
                | PdfObject::Bool(_)
                | PdfObject::Integer(_)
                | PdfObject::Real(_)
                | PdfObject::Ref(_) => {
                    total = total.saturating_add(24);
                }
                PdfObject::Name(n) => {
                    total = total.saturating_add(32).saturating_add(n.0.len());
                }
                PdfObject::String(s) => {
                    total = total.saturating_add(32).saturating_add(s.as_bytes().len());
                }
                PdfObject::Array(items) => {
                    total = total.saturating_add(32);
                    for item in items {
                        stack.push(Node::Obj(item));
                    }
                }
                PdfObject::Dict(d) => stack.push(Node::Dict(d)),
                PdfObject::Stream(stream) => {
                    total = total.saturating_add(64).saturating_add(stream.data.len());
                    stack.push(Node::Dict(&stream.dict));
                }
            },
        }
    }
    total
}

fn estimate_pdf_dict_bytes_iter(dict: &PdfDict) -> usize {
    dict.0.iter().fold(64usize, |acc, (k, v)| {
        acc.saturating_add(32)
            .saturating_add(k.0.len())
            .saturating_add(estimate_pdf_object_bytes_iter(v))
    })
}

fn estimate_type3_resource_scope_bytes(scope: &CachedType3ResourceScope) -> usize {
    let mut total = 64usize;
    if let Some(res) = scope.parsed_resources.as_ref() {
        total = total.saturating_add(256);
        let map_entries = |m: &HashMap<String, ObjectId>| -> usize {
            m.keys().fold(0usize, |acc, k| {
                acc.saturating_add(48).saturating_add(k.len())
            })
        };
        total = total
            .saturating_add(map_entries(&res.fonts))
            .saturating_add(map_entries(&res.xobjects))
            .saturating_add(map_entries(&res.ext_g_state))
            .saturating_add(map_entries(&res.color_spaces))
            .saturating_add(map_entries(&res.patterns))
            .saturating_add(map_entries(&res.shadings))
            .saturating_add(map_entries(&res.properties));
        for (k, d) in &res.ext_g_state_inline {
            total = total
                .saturating_add(48)
                .saturating_add(k.len())
                .saturating_add(estimate_pdf_dict_bytes_iter(d));
        }
        for (k, o) in &res.color_spaces_inline {
            total = total
                .saturating_add(48)
                .saturating_add(k.len())
                .saturating_add(estimate_pdf_object_bytes_iter(o));
        }
        for (k, o) in &res.shadings_inline {
            total = total
                .saturating_add(48)
                .saturating_add(k.len())
                .saturating_add(estimate_pdf_object_bytes_iter(o));
        }
        for (k, d) in &res.properties_inline {
            total = total
                .saturating_add(48)
                .saturating_add(k.len())
                .saturating_add(estimate_pdf_dict_bytes_iter(d));
        }
    }
    for binding in &scope.font_bindings {
        match binding {
            CachedFontBinding::PageSlot(name) => {
                total = total.saturating_add(32).saturating_add(name.len());
            }
            CachedFontBinding::CacheSlot(name, slot) => {
                total = total
                    .saturating_add(64)
                    .saturating_add(name.len())
                    .saturating_add(slot.len());
            }
        }
    }
    total
}

/// Colour-resolution qualifier for reusing decoded pixels of an inline image
/// inside a Type 3 glyph. Images whose `/ColorSpace` comes from an inline
/// resource entry are not cached because distinct enclosing scopes can bind
/// the same resource name to different inline palettes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Type3InlineColorKey {
    /// `/ImageMask true` stencil tinted with the active fill RGB.
    Mask([u8; 3]),
    /// Non-mask inline image with a self-contained `/ColorSpace` (`cs_ref: None`)
    /// or an indirect `/ColorSpace` resource (`cs_ref: Some(id)`), qualified by
    /// the effective rendering intent.
    Color {
        cs_ref: Option<ObjectId>,
        intent: zpdf_color::RenderIntent,
    },
}

#[derive(Debug, Clone)]
struct GraphicsState {
    ctm: Matrix,
    fill_color: Color,
    stroke_color: Color,
    fill_alpha: f32,
    stroke_alpha: f32,
    line_width: f32,
    line_cap: LineCap,
    line_join: LineJoin,
    miter_limit: f32,
    dash: Option<DashPattern>,
    font_name: String,
    font_size: f32,
    char_spacing: f32,
    word_spacing: f32,
    h_scaling: f32,
    leading: f32,
    rise: f32,
    render_mode: u8,
    clip_depth: u32,
    /// Page-space bounding box of the intersection of all active clip paths
    /// (`None` = unclipped / full page). Used to bound work that would otherwise
    /// cover the whole page — notably `sh` shading rasterization.
    clip_bounds: Option<Rect>,
    /// Device-pixel footprint of enforced clip masks currently on the display
    /// list (`None` = no enforced clip mask is active yet). Used solely for
    /// clip elision; elision requires `Some(_)` so removing a redundant clip
    /// never turns a masked draw into an unmasked one.
    clip_elision_bounds: Option<DeviceClipBounds>,
    fill_cs: ActiveColorSpace,
    stroke_cs: ActiveColorSpace,
    fill_pattern: Option<PatternPaint>,
    stroke_pattern: Option<PatternPaint>,
    blend_mode: BlendMode,
    /// ExtGState /SMask currently in effect (None = no soft mask).
    soft_mask: Option<SoftMask>,
    /// Colour rendering intent (`ri` operator / ExtGState /RI). Applied when
    /// compiling ICCBased profiles into transforms.
    rendering_intent: zpdf_color::RenderIntent,
    /// ExtGState /op — nonstroking (fill/text-fill/image) overprint (8.6.7).
    fill_overprint: bool,
    /// ExtGState /OP — stroking (and text-stroke) overprint (8.6.7).
    stroke_overprint: bool,
    /// ExtGState /OPM — overprint mode (0 = knockout zero, 1 = nonzero).
    overprint_mode: u8,
    /// CMYK colorant projection of `fill_color` (None = not a colorant space).
    fill_colorant: Option<Colorant>,
    /// CMYK colorant projection of `stroke_color`.
    stroke_colorant: Option<Colorant>,
    /// True once a non-suppressed nonstroking color operator ran in the active
    /// graphics state (used in Type 3 `d0` CharProcs to distinguish explicit
    /// glyph colors from the inherited text paint; saved/restored by `q`/`Q`).
    fill_color_explicit: bool,
    /// True once a non-suppressed stroking color operator ran in the active
    /// graphics state (saved/restored by `q`/`Q`).
    stroke_color_explicit: bool,
}

impl Default for GraphicsState {
    fn default() -> Self {
        Self {
            ctm: Matrix::identity(),
            fill_color: Color::black(),
            stroke_color: Color::black(),
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            line_width: 1.0,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: None,
            font_name: String::new(),
            font_size: 12.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            h_scaling: 100.0,
            leading: 0.0,
            rise: 0.0,
            render_mode: 0,
            clip_depth: 0,
            clip_bounds: None,
            clip_elision_bounds: None,
            fill_cs: ActiveColorSpace::DeviceGray,
            stroke_cs: ActiveColorSpace::DeviceGray,
            fill_pattern: None,
            stroke_pattern: None,
            blend_mode: BlendMode::Normal,
            soft_mask: None,
            rendering_intent: zpdf_color::RenderIntent::RelativeColorimetric,
            fill_overprint: false,
            stroke_overprint: false,
            overprint_mode: 0,
            fill_colorant: Some(Colorant {
                cmyk: [0.0, 0.0, 0.0, 1.0],
                cmyk_device: false,
            }),
            stroke_colorant: Some(Colorant {
                cmyk: [0.0, 0.0, 0.0, 1.0],
                cmyk_device: false,
            }),
            fill_color_explicit: false,
            stroke_color_explicit: false,
        }
    }
}

/// Which category of interpret work a step belongs to, for
/// [`InterpretStats`]'s per-category buckets.
///
/// Deliberately coarse: the question these buckets answer is *which kind of
/// content* a page spends its interpret time on (an image page spending 80 % in
/// `Image` is a different problem from one spreading it across `Path` and
/// `Paint`), not which handler is hot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    Path,
    Paint,
    Text,
    Image,
    Form,
    Color,
}

impl Work {
    /// The category an operator belongs to, or `None` for operators not worth a
    /// clock read.
    ///
    /// Only operators whose handlers do real work are listed. `q`/`Q`/`gs`/`cm`
    /// and the other state setters are the *majority* of the operators on most
    /// pages and are individually free, so attributing them would buy a lot of
    /// clock reads and no information.
    ///
    /// `Do` is absent on purpose: whether it draws an image or a form is known
    /// only once the XObject is resolved, so those two categories are timed at
    /// that dispatch point instead.
    fn of_operator(op: &str) -> Option<Self> {
        Some(match op {
            "m" | "l" | "c" | "v" | "y" | "re" | "h" => Self::Path,
            "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "S" | "s" | "n" | "W" | "W*" => {
                Self::Paint
            }
            "BT" | "ET" | "Td" | "TD" | "Tm" | "T*" | "Tf" | "Tj" | "TJ" | "'" | "\"" => Self::Text,
            "cs" | "CS" | "sc" | "scn" | "SC" | "SCN" | "g" | "G" | "rg" | "RG" | "k" | "K" => {
                Self::Color
            }
            _ => return None,
        })
    }
}

impl<'a> ContentInterpreter<'a> {
    pub fn new(page_rect: Rect) -> Self {
        let limits = ParseLimits::default();
        Self {
            state_stack: Vec::new(),
            current: GraphicsState::default(),
            display_list: DisplayList::new(page_rect),
            current_path: Path::new(),
            operand_stack: Vec::new(),
            text_active: false,
            text_matrix: Matrix::identity(),
            text_line_matrix: Matrix::identity(),
            text_clip_accum: None,
            text_clip_requested: false,
            pending_clip: None,
            text_clip_unavailable: false,
            font_cache: None,
            shared_font_doc: None,
            current_font_id: None,
            reported_font_fallbacks: Vec::new(),
            reported_font_outline_misses: Vec::new(),
            reported_font_text_misses: Vec::new(),
            embedded_glyph_health: Vec::new(),
            file: None,
            resources: None,
            image_cache: None,
            skip_images: false,
            skip_type3_painting: false,
            max_image_cache_bytes: limits.max_image_cache_bytes,
            max_font_cache_bytes: limits.max_font_cache_bytes,
            parse_limits: limits,
            icc_cache: None,
            output_intent_cmyk: None,
            image_obj_cache: HashMap::new(),
            image_mask_cache: HashMap::new(),
            image_mask_sizes: HashMap::new(),
            image_sources: HashMap::new(),
            retained_image_bytes: 0,
            image_downscale_scale: 0.0,
            clip_elision_scale: 0.0,
            rejected_image_objects: HashSet::new(),
            shading_cache: HashMap::new(),
            form_content_cache: HashMap::new(),
            tiling_pattern_cache: HashMap::new(),
            tiling_cell_cache: HashMap::new(),
            tiling_cell_cache_entries: 0,
            tiling_inherited_pattern_scopes: Vec::new(),
            retained_detached_commands: 0,
            nesting_limit_hit: false,
            max_depth_reached: 0,
            shading_pattern_cache: HashMap::new(),
            content_cache_bytes: 0,
            soft_mask_cache: HashMap::new(),
            form_font_overrides: Vec::new(),
            font_repair: Default::default(),
            font_override: false,
            form_resources: Vec::new(),
            form_resource_scopes: Vec::new(),
            next_resource_scope_id: 0,
            form_depth: 0,
            active_forms: Vec::new(),
            active_tiling_patterns: Vec::new(),
            active_type3_glyphs: Vec::new(),
            type3_font_resources: HashMap::new(),
            type3_shared_resources: HashMap::new(),
            type3_glyph_resources: HashMap::new(),
            type3_resource_cache_bytes: 0,
            max_type3_resource_cache_bytes: MAX_TYPE3_RESOURCE_CACHE_BYTES,
            type3_oc_suppressed_mark: false,
            type3_image_dropped: false,
            type3_local_image_paint: false,
            type3_caller_fill: None,
            type3_inline_ctx: None,
            type3_inline_image_cache: HashMap::new(),
            type3_inline_fallback_cache: HashMap::new(),
            type3_inline_xobject_cache: HashMap::new(),
            type3_inline_xobject_sizes: HashMap::new(),
            type3_rejected_xobject_footprints: HashSet::new(),
            type3_inline_fallback_bytes: 0,
            state_floor: 0,
            text_sink: None,
            span_streams: None,
            text_geometry: false,
            text_glyphs_collected: 0,
            extracted_text_bytes: 0,
            rule_sink: None,
            base_ctm: Matrix::identity(),
            base_state: GraphicsState::default(),
            base_font_id: None,
            shading_space_to_page: Matrix::identity(),
            page_base_ctm: Matrix::identity(),
            suppress_color_ops: false,
            type3_glyph: false,
            type3_resources: None,
            type3_resource_stack: Vec::new(),
            oc_config: None,
            annotations: None,
            mc_depth: 0,
            compatibility_depth: 0,
            mc_stack: Vec::new(),
            active_actual_text: None,
            struct_actual_text_mcids: std::collections::HashSet::new(),
            current_mc_stream: Some(None),
            enclosing_struct_actual_text: false,
            actual_text_bytes: 0,
            max_actual_text_bytes: 16 << 20,
            oc_hidden_from: None,
            max_commands: DEFAULT_MAX_COMMANDS,
            anisotropic_stroke_points: 0,
            max_anisotropic_stroke_points: DEFAULT_MAX_ANISOTROPIC_STROKE_POINTS,
            ops_executed: 0,
            max_ops: DEFAULT_MAX_OPS,
            deadline: None,
            timed_out: false,
            last_clock_op: 0,
            cmyk_cache: RefCell::new(None),
            cs_cache: RefCell::new(HashMap::new()),
            max_operand_stack_depth: 10_000, // M3: Match ParseLimits::default()
            max_graphics_state_depth: 256,
            max_marked_content_depth: 128,
            resource_limit_hit: false,
            stats: InterpretStats::default(),
            work_timing: false,
        }
    }

    /// Override the emitted-command ceiling (see [`DEFAULT_MAX_COMMANDS`]).
    /// Collect the per-category work buckets in [`InterpretStats`] — which kind
    /// of content the page spends its interpret time on.
    ///
    /// Off by default. These clocks sit on every attributed operator rather than
    /// once per page, so a page with hundreds of thousands of path operators
    /// would pay for the measurement; the whole-stage and shading timings are
    /// always collected and need no flag.
    pub fn with_work_timing(mut self, enabled: bool) -> Self {
        self.work_timing = enabled;
        self
    }

    /// Interpret a Type 3 glyph CharProc stream (ISO 32000-1 §9.6.5):
    /// `d1` suppresses color operators so every mark inherits the enclosing
    /// text paint (`Paint::Inherited` / `is_image_mask: true`), whereas `d0`
    /// preserves explicit fill/stroke colors (`Paint::Solid`) and explicit
    /// stencil colors while still inheriting before any color operator runs.
    pub fn for_type3_glyph(mut self) -> Self {
        self.type3_glyph = true;
        self
    }

    /// Attach pre-resolved Type 3 font `/Resources` so standalone CharProc
    /// replay can resolve named XObjects (`/Name Do`), color spaces, and
    /// ExtGStates without a live `PdfFile`.
    pub fn with_type3_resources(
        mut self,
        resources: Option<&'a zpdf_font::Type3Resources>,
    ) -> Self {
        self.type3_resources = resources;
        self
    }

    fn lookup_type3_xobject(&self, name: &str) -> Option<zpdf_font::Type3XObject> {
        for res in self.type3_resource_stack.iter().rev() {
            if let Some(xobj) = res.xobjects.get(name) {
                return Some(xobj.clone());
            }
        }
        self.type3_resources
            .and_then(|res| res.xobjects.get(name).cloned())
    }

    fn lookup_type3_colorspace(&self, name: &str) -> Option<PdfObject> {
        for res in self.type3_resource_stack.iter().rev() {
            if let Some(cs) = res.color_spaces.get(name) {
                return Some(cs.clone());
            }
        }
        self.type3_resources
            .and_then(|res| res.color_spaces.get(name).cloned())
    }

    fn lookup_type3_shading(&self, name: &str) -> Option<std::sync::Arc<zpdf_font::Type3Shading>> {
        for res in self.type3_resource_stack.iter().rev() {
            if let Some(sh) = res.shadings.get(name) {
                return Some(std::sync::Arc::clone(sh));
            }
        }
        self.type3_resources
            .and_then(|res| res.shadings.get(name).cloned())
    }

    fn parse_type3_function(
        func: &std::sync::Arc<zpdf_font::Type3Function>,
    ) -> Option<zpdf_color::PdfFunction> {
        type FuncEntry = (zpdf_core::PdfDict, Option<std::sync::Arc<[u8]>>);
        fn flatten(
            f: &std::sync::Arc<zpdf_font::Type3Function>,
            table: &mut Vec<FuncEntry>,
            seen: &mut std::collections::HashMap<usize, ObjectId>,
        ) -> FuncEntry {
            match f.as_ref() {
                zpdf_font::Type3Function::Stream { dict, data } => {
                    (dict.clone(), Some(std::sync::Arc::clone(data)))
                }
                zpdf_font::Type3Function::Dict { dict, children } => {
                    let mut out = dict.clone();
                    if !children.is_empty() {
                        let mut refs = Vec::with_capacity(children.len());
                        for child in children {
                            let ptr = std::sync::Arc::as_ptr(child) as usize;
                            let id = if let Some(&existing) = seen.get(&ptr) {
                                existing
                            } else {
                                let entry = flatten(child, table, seen);
                                let id = ObjectId(table.len() as u32, 0);
                                table.push(entry);
                                seen.insert(ptr, id);
                                id
                            };
                            refs.push(PdfObject::Ref(id));
                        }
                        out.insert(zpdf_core::PdfName::new("Functions"), PdfObject::Array(refs));
                    }
                    (out, None)
                }
            }
        }
        let mut table = Vec::new();
        let mut seen = std::collections::HashMap::new();
        let (root_dict, root_data) = flatten(func, &mut table, &mut seen);
        let mut resolve = |id: ObjectId| {
            table
                .get(id.0 as usize)
                .map(|(d, b)| (PdfObject::Dict(d.clone()), b.as_ref().map(|s| s.to_vec())))
        };
        zpdf_color::PdfFunction::parse(&root_dict, root_data.as_deref(), &mut resolve)
    }

    fn lookup_type3_ext_gstate(
        &self,
        name: &str,
    ) -> Option<(zpdf_core::PdfDict, Option<zpdf_font::Type3SoftMask>)> {
        for res in self.type3_resource_stack.iter().rev() {
            let dict = res.ext_g_states.get(name);
            let smask = res.ext_g_state_smasks.get(name);
            if dict.is_some() || smask.is_some() {
                return Some((dict.cloned().unwrap_or_default(), smask.cloned()));
            }
        }
        self.type3_resources.and_then(|res| {
            let dict = res.ext_g_states.get(name);
            let smask = res.ext_g_state_smasks.get(name);
            if dict.is_some() || smask.is_some() {
                Some((dict.cloned().unwrap_or_default(), smask.cloned()))
            } else {
                None
            }
        })
    }

    /// Start a stopwatch for one step of interpret work, or `None` when it is
    /// not worth timing (an unattributed operator, or collection off). Pair with
    /// [`Self::tock_work`].
    fn start_work(&self, work: Option<Work>) -> Option<(Work, zpdf_core::time::Instant)> {
        if !self.work_timing {
            return None;
        }
        work.map(|work| (work, zpdf_core::time::Instant::now()))
    }

    /// Close a [`Self::start_work`] bucket. A no-op when `started` is `None`, so
    /// every call site is unconditional in the code and free in production.
    fn tock_work(&mut self, started: Option<(Work, zpdf_core::time::Instant)>) {
        let Some((work, started)) = started else {
            return;
        };
        let ns = started.elapsed().as_nanos() as u64;
        let stats = &mut self.stats;
        match work {
            Work::Path => {
                stats.path_ns += ns;
                stats.path_ops += 1;
            }
            Work::Paint => {
                stats.paint_ns += ns;
                stats.paint_ops += 1;
            }
            Work::Text => {
                stats.text_ns += ns;
                stats.text_ops += 1;
            }
            Work::Image => {
                stats.image_ns += ns;
                stats.image_ops += 1;
            }
            Work::Form => {
                stats.form_ns += ns;
                stats.form_ops += 1;
            }
            Work::Color => {
                stats.color_ns += ns;
                stats.color_ops += 1;
            }
        }
    }

    pub fn with_command_limit(mut self, max_commands: usize) -> Self {
        self.max_commands = max_commands;
        self
    }

    /// Override the cumulative page-wide point budget for expanded anisotropic
    /// stroke outlines (including detached tiling cells and soft masks).
    pub fn with_anisotropic_stroke_point_limit(mut self, max_points: usize) -> Self {
        self.max_anisotropic_stroke_points = max_points;
        self
    }

    /// Override the operand stack depth limit (M3 security fix).
    pub fn with_operand_stack_limit(mut self, max_depth: usize) -> Self {
        self.max_operand_stack_depth = max_depth;
        self
    }

    /// Override the retained graphics-state (`q`) depth limit.
    pub fn with_graphics_state_limit(mut self, max_depth: usize) -> Self {
        self.max_graphics_state_depth = max_depth;
        self
    }

    /// Override the marked-content (BMC/BDC) nesting limit.
    pub fn with_marked_content_limit(mut self, max_depth: usize) -> Self {
        self.max_marked_content_depth = max_depth;
        self
    }

    /// Supply stream-scoped `(owning_stream, mcid)` pairs subsumed by
    /// structure-element `/ActualText` replacements on this page (`None` for
    /// the page stream; `Some(id)` for a Form XObject stream referenced by
    /// `/MCR /Stm`).
    #[doc(hidden)]
    pub fn with_struct_actual_text_mcids(
        mut self,
        mcids: std::collections::HashSet<(Option<ObjectId>, i64)>,
    ) -> Self {
        self.struct_actual_text_mcids = mcids;
        self
    }

    /// Stop interpreting at `deadline` rather than a fresh interpret budget
    /// from the start — for a nested program (e.g. a Type 3 glyph drawn by a
    /// render backend) that must not outlive the page it is part of.
    pub fn with_deadline(mut self, deadline: zpdf_core::time::Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Apply the content-related limits from a document's [`ParseLimits`].
    ///
    /// [`Self::with_document`] calls this automatically. It remains public for
    /// callers that interpret standalone streams while enforcing the same
    /// policy as a parsed document.
    pub fn with_limits(mut self, limits: &ParseLimits) -> Self {
        self.apply_limits(limits);
        self
    }

    fn apply_limits(&mut self, limits: &ParseLimits) {
        self.max_ops = limits.max_page_operators;
        self.max_operand_stack_depth = limits.max_operand_stack_depth as usize;
        self.max_graphics_state_depth = limits.max_graphics_state_depth as usize;
        self.max_marked_content_depth = limits.max_marked_content_depth as usize;
        self.max_actual_text_bytes = (limits.max_decoded_stream_bytes.min(16 << 20)) as usize;
        self.max_image_cache_bytes = limits.max_image_cache_bytes;
        self.max_font_cache_bytes = limits.max_font_cache_bytes;
        self.parse_limits = limits.clone();
    }

    /// True once the page has hit any anti-hang ceiling: emitted commands,
    /// executed operators, or the wall-clock deadline. Checked by the top-level
    /// loop and the recursive form/tiling emitters so a runaway page is
    /// truncated rather than hanging — whether it explodes in emitted commands,
    /// raw call fanout, or per-call time (e.g. per-cell shading rasterization).
    /// The operator loop samples the clock every 16 operators. Long text
    /// operations also check it while processing their glyphs.
    fn over_budget(&mut self) -> bool {
        if self.resource_limit_hit
            || self
                .retained_detached_commands
                .saturating_add(self.display_list.commands.len())
                >= self.max_commands
            || self.ops_executed >= self.max_ops
        {
            self.stats.diagnostics.resource_limit_reached = true;
            return true;
        }
        if !self.timed_out {
            if let Some(deadline) = self.deadline {
                if self.ops_executed.wrapping_sub(self.last_clock_op) >= 16 {
                    self.last_clock_op = self.ops_executed;
                    if zpdf_core::time::Instant::now() >= deadline {
                        self.timed_out = true;
                        tracing::warn!("content interpret exceeded time budget; truncating page");
                    }
                }
            }
        }
        self.timed_out
    }

    /// Whether a limit has ended interpretation. Unlike [`Self::over_budget`]
    /// this reads no clock and records nothing.
    fn cut_short(&self) -> bool {
        self.timed_out
            || self.resource_limit_hit
            || self
                .retained_detached_commands
                .saturating_add(self.display_list.commands.len())
                >= self.max_commands
            || self.ops_executed >= self.max_ops
    }

    /// M3 Fix: Check operand stack depth before pushing. Returns true if the
    /// push would exceed the limit.
    fn operand_stack_full(&self) -> bool {
        self.operand_stack.len() >= self.max_operand_stack_depth
    }

    fn push_operand(&mut self, obj: PdfObject) -> bool {
        if self.operand_stack_full() {
            tracing::warn!(
                "operand stack exceeded {} entries; truncating page",
                self.max_operand_stack_depth
            );
            self.resource_limit_hit = true;
            return false;
        }
        self.operand_stack.push(obj);
        true
    }

    /// Look a value up through the form-resources stack (innermost first),
    /// falling back to the page-level resources, and return the defining
    /// scope's default `base_ctm` and scope identity alongside the resource.
    fn lookup_res_with_base_ctm<T>(
        &self,
        get: impl Fn(&ResourceDict) -> Option<T>,
    ) -> Option<(T, Matrix, InheritedPatternScope)> {
        for (idx, (r, scope_base_ctm)) in self.form_resources.iter().enumerate().rev() {
            if let Some(v) = get(r.as_ref()) {
                return Some((
                    v,
                    *scope_base_ctm,
                    InheritedPatternScope::EnclosingForm(idx),
                ));
            }
        }
        let page_ctm = if self.form_depth == 0 {
            self.base_ctm
        } else {
            self.page_base_ctm
        };
        self.resources
            .and_then(get)
            .map(|v| (v, page_ctm, InheritedPatternScope::Page))
    }

    /// Record that a pattern from `scope` was resolved, marking every currently
    /// executing tiling cell whose enclosing scope contains `scope` so its cache
    /// entry tracks that scope's relative transform.
    fn record_tiling_inherited_pattern_scope(&mut self, scope: InheritedPatternScope) {
        for (outer_scope_len, used) in &mut self.tiling_inherited_pattern_scopes {
            let is_enclosing = match scope {
                InheritedPatternScope::Page => true,
                InheritedPatternScope::EnclosingForm(idx) => idx < *outer_scope_len,
            };
            if is_enclosing && !used.contains(&scope) {
                used.push(scope);
            }
        }
    }

    /// Look a value up through the form-resources stack (innermost first),
    /// falling back to the page-level resources.
    fn lookup_res<T>(&self, get: impl Fn(&ResourceDict) -> Option<T>) -> Option<T> {
        self.lookup_res_with_base_ctm(get).map(|(v, _, _)| v)
    }

    /// Resolve a `/ColorSpace` resource by name, checking both direct (inline)
    /// and indirect entries within each scope before advancing to its parent,
    /// with fallback to pre-resolved standalone Type 3 font resources.
    fn lookup_color_space_resource(&self, name: &str) -> Option<PdfObject> {
        self.lookup_res(|r| {
            r.color_spaces_inline
                .get(name)
                .cloned()
                .or_else(|| r.color_spaces.get(name).copied().map(PdfObject::Ref))
        })
        .or_else(|| self.lookup_type3_colorspace(name))
    }

    /// Resolve a `/Properties` resource by name, checking both direct (inline)
    /// and indirect entries within each scope before advancing to its parent.
    fn lookup_property_resource(&self, name: &str) -> Option<PdfObject> {
        self.lookup_res(|r| {
            r.properties_inline
                .get(name)
                .cloned()
                .map(PdfObject::Dict)
                .or_else(|| r.properties.get(name).copied().map(PdfObject::Ref))
        })
    }

    /// Apply the page `/Rotate` entry (clockwise degrees, normalized to
    /// 0/90/180/270) by baking a rotation into the base CTM and swapping the
    /// page rect for the quarter turns. All content then renders pre-rotated, so
    /// the render backends need no rotation logic. Non-quadrant values are
    /// ignored (treated as 0).
    pub fn with_page_rotation(mut self, rotate: i32) -> Self {
        let r = rotate.rem_euclid(360);
        if r == 0 {
            return self;
        }
        let rect = self.display_list.page_rect;
        let (w, h) = (rect.width(), rect.height());
        let (x0, y0) = (rect.x0, rect.y0);
        // Matrix::new(a,b,c,d,e,f) maps (x,y) -> (a*x+c*y+e, b*x+d*y+f). Each
        // turn maps the W×H page into the rotated box with origin bottom-left.
        // Translate the effective-box origin to zero as part of the rotation;
        // otherwise a CropBox/MediaBox whose origin is not (0,0) is shifted.
        let (base, rotated) = match r {
            90 => (
                Matrix::new(0.0, -1.0, 1.0, 0.0, -y0, w + x0),
                Rect::new(0.0, 0.0, h, w),
            ),
            180 => (
                Matrix::new(-1.0, 0.0, 0.0, -1.0, w + x0, h + y0),
                Rect::new(0.0, 0.0, w, h),
            ),
            270 => (
                Matrix::new(0.0, 1.0, -1.0, 0.0, h + y0, -x0),
                Rect::new(0.0, 0.0, h, w),
            ),
            _ => return self,
        };
        self.current.ctm = base;
        self.base_ctm = base;
        self.display_list.page_rect = rotated;
        self
    }

    /// Translate the initial CTM by `(dx, dy)`, exactly as a leading
    /// `1 0 0 1 dx dy cm` operator would (concatenated onto the current CTM,
    /// leaving `base_ctm` untouched). This lets a caller apply a CropBox-origin
    /// shift — e.g. AutoCAD pages whose content lives at a non-zero MediaBox/
    /// CropBox origin — without prepending that operator to a *copy* of the whole
    /// content stream (on a 20+ MB stream that duplicate is pure waste). Call
    /// after [`with_page_rotation`](Self::with_page_rotation).
    pub fn with_content_translation(mut self, dx: f64, dy: f64) -> Self {
        self.current.ctm = self.current.ctm.concat(&Matrix::translate(dx, dy));
        self
    }

    /// Convert default page user space to physical points. Call after
    /// [`Self::with_page_rotation`], before interpreting content. Annotation
    /// appearances and patterns use the same scaled base transform.
    pub fn with_page_user_unit(mut self, unit: f64) -> Self {
        if unit.is_finite() && unit > 0.0 && unit <= 75_000.0 {
            let scale = Matrix::scale(unit, unit);
            self.current.ctm = scale.concat(&self.current.ctm);
            self.base_ctm = scale.concat(&self.base_ctm);
            let r = self.display_list.page_rect;
            self.display_list.page_rect =
                Rect::new(r.x0 * unit, r.y0 * unit, r.x1 * unit, r.y1 * unit);
        }
        self
    }

    pub fn with_fonts(mut self, cache: &'a mut FontCache) -> Self {
        self.font_cache = Some(cache);
        self
    }

    /// Load the fonts Form XObjects and ExtGState `/Font` entries reference
    /// through `doc`'s shared font cache ([`PdfDocument::shared_font`]), so a
    /// form font drawn on every page (a header, a watermark) is parsed once
    /// per document instead of once per page. Applies while the interpreted
    /// file is `doc.file()` and the font cache uses the document's provider;
    /// otherwise fonts are parsed into the page cache as without it.
    ///
    /// [`PdfDocument::shared_font`]: zpdf_document::PdfDocument::shared_font
    pub fn with_shared_fonts(mut self, doc: &'a zpdf_document::PdfDocument) -> Self {
        self.shared_font_doc = Some(doc);
        self
    }

    /// Prepare images for the CPU backend rendering at `render_scale`: an image
    /// that backend would box-filter on draw (an axis below 0.5x of its
    /// samples) is stored already filtered to that draw's footprint, so the
    /// image cache does not retain a scan far larger than the area it fills.
    ///
    /// Defined output: while no image is dropped for the image byte limit, the
    /// CPU backend rendering the display list at `render_scale` produces the
    /// same pixels as without this option. A copy is stored only if the
    /// backend samples it directly; every copy is filtered from the decoded
    /// source and cached by its size, so an image drawn under several
    /// transforms never reuses or refilters another draw's copy. Draws that
    /// cannot use a filtered copy use the source, which the backend filters on
    /// draw as it would without this option.
    ///
    /// Memory: an image's filtered copies never exceed its source size; once
    /// they would, or a draw is not minified, its source is cached and serves
    /// later sizes. A source is decoded at most twice; after the second decode
    /// it is retained when the image byte limit has room for it, counted
    /// against that limit, and otherwise cached if it fits. Near that
    /// limit the two modes can drop different images: this option can draw an
    /// image whose source does not fit, and since an image may hold its source
    /// plus filtered copies up to that size again, it can also drop a later
    /// image that the default would draw. Filtering and a second decode run
    /// here and count against the interpreter's time budget.
    ///
    /// Pass `CpuRenderer::raster_scale(page_rect, scale)`, which includes the
    /// renderer's page-pixel budget. Other consumers (GPU, SVG, PPTX, image
    /// extraction) need source pixels and must keep the default `0.0`, which
    /// disables the option.
    pub fn with_image_downscale(mut self, render_scale: f32) -> Self {
        self.image_downscale_scale = if render_scale.is_finite() {
            render_scale.max(0.0)
        } else {
            0.0
        };
        self
    }

    /// Elide axis-aligned rectangle clips that cover every affected device
    /// pixel of an already-active clip mask at `render_scale` (matching
    /// `CpuRenderer::raster_scale`).
    ///
    /// Defined output: while no clip is skipped for the CPU backend's clip
    /// mask-memory (`MAX_CLIP_MASK_BYTES`) or cumulative pixel-work
    /// (`MAX_CLIP_PIXEL_WORK`) budgets, rendering the display list at
    /// `render_scale` produces the exact same pixels as without this option. A
    /// covering clip is dropped only when an enforced clip mask is already
    /// active and the new rectangle encloses every affected device pixel of
    /// that mask within `tiny-skia`'s supersampled AA scanline range,
    /// preserving fractional page edges, antialiased shared clip edges, and
    /// `tiny-skia`'s masked `SourceOver` paint rounding bit-for-bit.
    ///
    /// Budgets: elided covering clips never allocate masks or charge
    /// cumulative clip pixel-work, so pages that would otherwise exhaust those
    /// CPU DoS budgets can retain and enforce later genuine clips instead of
    /// degrading to skipped clip frames (matching [`Self::with_image_downscale`],
    /// and never dropping a clip that the unelided stream would have enforced).
    /// Default `0.0` disables elision so scale-independent and GPU display
    /// lists retain every clip.
    pub fn with_clip_elision_scale(mut self, render_scale: f32) -> Self {
        self.clip_elision_scale = if render_scale.is_finite() && render_scale > 0.0 {
            render_scale
        } else {
            0.0
        };
        self
    }

    /// Collect decoded text spans (for the `text` command / extraction) in
    /// addition to building the display list. With no sink, no Unicode mapping
    /// is allocated. Spans carry glyph selection cells only when
    /// [`Self::with_text_geometry`] asks for them.
    pub fn with_text_sink(mut self, sink: &'a mut Vec<TextSpan>) -> Self {
        self.text_sink = Some(sink);
        self
    }

    /// Collect parallel owning-stream bindings for spans pushed into
    /// [`Self::with_text_sink`], used by structured text assembly.
    #[doc(hidden)]
    pub fn with_span_streams(mut self, sink: &'a mut Vec<crate::text::SpanStreamBinding>) -> Self {
        self.span_streams = Some(sink);
        self
    }

    /// Also record a positioned selection cell for every glyph of the spans
    /// the text sink collects ([`TextSpan::glyphs`]), for character selection
    /// and glyph-accurate search highlights. Off by default: plain text
    /// extraction reads only the decoded string and the span metrics, and each
    /// cell takes about 100 bytes. Has no effect without a text sink.
    pub fn with_text_geometry(mut self, enabled: bool) -> Self {
        self.text_geometry = enabled;
        self
    }

    /// Collect drawn ruled lines — thin axis-aligned strokes and thin filled
    /// rectangles, in page space — for ruled-line-aware table detection
    /// ([`crate::tables::detect_tables_with_rules`]). Like the text sink, this
    /// is extraction-only: it never changes the emitted display list.
    pub fn with_rule_sink(mut self, sink: &'a mut Vec<RuleLine>) -> Self {
        self.rule_sink = Some(sink);
        self
    }

    pub fn with_document(mut self, file: &'a PdfFile, resources: &'a ResourceDict) -> Self {
        self.apply_limits(file.limits());
        self.file = Some(file);
        self.resources = Some(resources);
        self
    }

    pub fn with_images(mut self, cache: &'a mut ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Skip all image decoding, including self-contained Type 3 inline bitmaps
    /// and shading rasters when no external `ImageCache` is attached.
    ///
    /// Intended for extraction-only callers (such as `ConversionMode::TextOnly`)
    /// that discard the display list and must not spend image-decode work or
    /// emit image-budget diagnostics.
    pub fn with_skip_images(mut self, skip: bool) -> Self {
        self.skip_images = skip;
        self
    }

    /// Skip visual replay of Type 3 glyph CharProc streams while preserving
    /// text advances and decoded text extraction.
    ///
    /// Intended for extraction-only callers (such as `ConversionMode::TextOnly`)
    /// that discard the display list so repeated vector glyphs cannot consume
    /// the page's shared operator budget.
    pub fn with_skip_type3_painting(mut self, skip: bool) -> Self {
        self.skip_type3_painting = skip;
        self
    }

    /// Override aggregate decoded-image cache admission for standalone use.
    pub fn with_image_cache_limit(mut self, max_bytes: u64) -> Self {
        self.max_image_cache_bytes = max_bytes;
        self
    }

    /// Honor the document's optional-content configuration: content in groups
    /// turned off (BDC /OC blocks, XObject /OC, annotation /OC) is skipped.
    pub fn with_optional_content(mut self, config: &'a zpdf_document::OcConfig) -> Self {
        self.oc_config = Some(config);
        self
    }

    /// Paint these annotations' appearance streams after the page content.
    pub fn with_annotations(mut self, annots: &'a [zpdf_document::Annotation]) -> Self {
        self.annotations = Some(annots);
        self
    }

    /// Inject the per-document ICC transform cache. With it, ICCBased colour
    /// spaces (vector and image) convert through their embedded profiles;
    /// without it (or when a profile is unusable) they keep the `/N`
    /// component-count fallback.
    pub fn with_colors(mut self, cache: &'a mut zpdf_color::IccCache) -> Self {
        self.icc_cache = Some(cache);
        self
    }

    /// Inject the document's output-intent default CMYK profile (already
    /// compiled and validated 4-channel by the caller, e.g. via
    /// [`output_intent_cmyk_profile`]). DeviceCMYK colours — vector fills,
    /// strokes, text, and raster images — then render through this ICC transform
    /// (the PDF/X characterized-printing-condition model) instead of the generic
    /// Adobe SWOP polynomial. Pass the page-level intent's profile when present,
    /// else the document-level one. An explicit embedded ICCBased colour space
    /// always keeps its own profile; this only substitutes for *DeviceCMYK*.
    pub fn with_output_intent_cmyk(
        mut self,
        profile: std::sync::Arc<zpdf_color::IccTransform>,
    ) -> Self {
        self.output_intent_cmyk = Some(profile);
        self
    }

    /// Interpret `content` into a [`DisplayList`].
    ///
    /// Discards the stage timings. Kept as the primary API so no existing caller
    /// changes; use [`Self::interpret_with_stats`] when measuring.
    pub fn interpret(self, content: &[u8]) -> DisplayList {
        self.interpret_with_stats(content).0
    }

    /// Like [`Self::interpret`], but also returns the interpret-stage timings.
    ///
    /// These are the only place a shading's cost is observable: shadings are
    /// rasterized in this stage and reach a render backend as a plain image.
    /// See [`InterpretStats`].
    pub fn interpret_with_stats(mut self, content: &[u8]) -> (DisplayList, InterpretStats) {
        let started = zpdf_core::time::Instant::now();
        // Arm the interpret wall-clock backstop (unless already set by a caller).
        if self.deadline.is_none() {
            self.deadline = Some(zpdf_core::time::Instant::now() + INTERPRET_BUDGET);
        }
        let mut tokenizer = ContentTokenizer::new(content);

        while let Some(token) = tokenizer.next_borrowed_recovering() {
            if self.over_budget() {
                tracing::warn!(
                    "content exceeded {} display commands; truncating page",
                    self.max_commands
                );
                break;
            }
            match token {
                ContentToken::Operand(obj) => {
                    if !self.push_operand(obj) {
                        break;
                    }
                }
                ContentToken::Operator(op) => {
                    // Per-category attribution, off unless
                    // `with_work_timing` asked for it. The nested form/mask
                    // loop below is deliberately *not* instrumented: its wall
                    // time is already inside `form_ns`, and re-attributing its
                    // operators would double count against that.
                    let work = self.start_work(Work::of_operator(&op));
                    self.execute_operator(&op);
                    self.tock_work(work);
                    self.operand_stack.clear();
                }
                ContentToken::InlineImage { dict, data } => {
                    let work = self.start_work(Some(Work::Image));
                    self.do_inline_image(dict, data);
                    self.tock_work(work);
                    self.operand_stack.clear();
                }
            }
        }

        self.stats.diagnostics.merge(tokenizer.diagnostics());
        self.close_marked_content();
        // Annotation appearances render independently of wherever the page
        // content stream left the graphics state (12.5.5): drop a `W` never
        // painted, rebalance any unclosed q-levels and top-level W clips, then
        // reset to the page's initial state (an unbalanced BDC /OC must not
        // blank annotations).
        self.pending_clip = None;
        while self.state_stack.len() > self.state_floor {
            for _ in 0..self.current.clip_depth {
                self.display_list.push(RenderCommand::PopClip);
            }
            if let Some(state) = self.state_stack.pop() {
                self.current = state;
            }
        }
        for _ in 0..self.current.clip_depth {
            self.display_list.push(RenderCommand::PopClip);
        }
        self.current = GraphicsState {
            ctm: self.base_ctm,
            ..GraphicsState::default()
        };
        self.mc_depth = 0;
        self.mc_stack.clear();
        self.oc_hidden_from = None;
        self.suppress_color_ops = false;

        self.paint_annotations();

        self.stats.font_repairs = self.font_repair.repairs;
        self.stats.operators_executed = self.ops_executed;
        self.stats.diagnostics.resource_limit_reached |= self.resource_limit_hit;
        self.stats.diagnostics.time_limit_reached |= self.timed_out;
        self.stats.font_repair_probes = self.font_repair.probes;
        self.stats.total_ns = started.elapsed().as_nanos() as u64;
        (self.display_list, self.stats)
    }

    pub fn command_count(&self) -> usize {
        self.display_list.commands.len()
    }

    fn pop_f64(&mut self) -> f64 {
        self.operand_stack
            .pop()
            .and_then(|o| o.as_f64().ok())
            .filter(|n| n.is_finite())
            .unwrap_or_else(|| {
                self.stats.diagnostics.malformed_content =
                    self.stats.diagnostics.malformed_content.saturating_add(1);
                0.0
            })
    }

    /// The top `N` operands of a path operator, in order. With fewer than
    /// `N` finite numbers the operator is malformed and is skipped (as pdf.js
    /// and hayro do); padding with zeros would draw segments to the origin.
    fn path_operands<const N: usize>(&mut self) -> Option<[f64; N]> {
        let len = self.operand_stack.len();
        let values = (len >= N)
            .then(|| &self.operand_stack[len - N..])
            .and_then(|top| {
                let mut out = [0.0; N];
                for (slot, operand) in out.iter_mut().zip(top) {
                    *slot = operand.as_f64().ok().filter(|n| n.is_finite())?;
                }
                Some(out)
            });
        match values {
            Some(values) => {
                self.operand_stack.truncate(len - N);
                Some(values)
            }
            None => {
                self.stats.diagnostics.malformed_content =
                    self.stats.diagnostics.malformed_content.saturating_add(1);
                None
            }
        }
    }

    fn pop_name(&mut self) -> String {
        self.operand_stack
            .pop()
            .and_then(|o| match o {
                PdfObject::Name(n) => Some(n.0),
                _ => None,
            })
            .unwrap_or_else(|| {
                self.stats.diagnostics.malformed_content =
                    self.stats.diagnostics.malformed_content.saturating_add(1);
                String::new()
            })
    }

    fn pop_string_bytes(&mut self) -> Vec<u8> {
        self.operand_stack
            .pop()
            .and_then(|o| match o {
                PdfObject::String(s) => Some(s.0),
                _ => None,
            })
            .unwrap_or_else(|| {
                self.stats.diagnostics.malformed_content =
                    self.stats.diagnostics.malformed_content.saturating_add(1);
                Vec::new()
            })
    }

    fn execute_operator(&mut self, op: &str) {
        // Count every operator toward the global work budget so exponential
        // form/pattern fanout (which emits few commands per call) still stops.
        self.ops_executed = self.ops_executed.saturating_add(1);

        // A clip still pending here was not followed by a painting operator
        // (malformed); apply it before anything else can observe the state.
        if !matches!(
            op,
            "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "n"
        ) {
            self.flush_pending_clip();
        }

        // Uncolored (PaintType 2) pattern cells and Type 3 `d1` glyphs paint
        // exclusively with the enclosing color; their own color, rendering-
        // intent, and shading operators are ignored (ISO 32000-1 §8.7.3.3,
        // §9.6.5.4).
        if self.suppress_color_ops
            && matches!(
                op,
                "g" | "G"
                    | "rg"
                    | "RG"
                    | "k"
                    | "K"
                    | "cs"
                    | "CS"
                    | "sc"
                    | "scn"
                    | "SC"
                    | "SCN"
                    | "ri"
                    | "sh"
            )
        {
            return;
        }
        match op {
            // -- Graphics state --
            "q" => {
                if self.state_stack.len() >= self.max_graphics_state_depth {
                    tracing::warn!(
                        "graphics-state stack exceeded {} entries; truncating page",
                        self.max_graphics_state_depth
                    );
                    self.resource_limit_hit = true;
                    return;
                }
                self.state_stack.push(self.current.clone());
                self.current.clip_depth = 0;
            }
            "Q" => {
                // Pop any clips that were pushed in this state level
                for _ in 0..self.current.clip_depth {
                    self.display_list.push(RenderCommand::PopClip);
                }
                if self.state_stack.len() > self.state_floor {
                    if let Some(state) = self.state_stack.pop() {
                        self.current = state;
                    }
                } else {
                    // Unbalanced Q at this nesting level (page bottom or a
                    // form's own saved state): keep the current state, but the
                    // clips counted above are gone. Clear clip_elision_bounds
                    // if any clip was just popped so later clips cannot be
                    // elided against a stale tighter mask.
                    if self.current.clip_depth > 0 {
                        self.current.clip_elision_bounds = None;
                    }
                    self.current.clip_depth = 0;
                }
            }
            "cm" => {
                let f = self.pop_f64();
                let e = self.pop_f64();
                let d = self.pop_f64();
                let c = self.pop_f64();
                let b = self.pop_f64();
                let a = self.pop_f64();
                let m = Matrix::new(a, b, c, d, e, f);
                let next = self.current.ctm.concat(&m);
                if next.is_finite() {
                    self.current.ctm = next;
                } else {
                    self.stats.diagnostics.malformed_content =
                        self.stats.diagnostics.malformed_content.saturating_add(1);
                    tracing::warn!("content transform overflowed; ignoring cm operator");
                }
            }
            "w" => self.current.line_width = self.pop_f64() as f32,
            "J" => {
                self.current.line_cap = match self.pop_f64() as u8 {
                    1 => LineCap::Round,
                    2 => LineCap::Square,
                    _ => LineCap::Butt,
                };
            }
            "j" => {
                self.current.line_join = match self.pop_f64() as u8 {
                    1 => LineJoin::Round,
                    2 => LineJoin::Bevel,
                    _ => LineJoin::Miter,
                };
            }
            "M" => self.current.miter_limit = self.pop_f64() as f32,
            "d" => {
                let phase = self.pop_f64() as f32;
                if let Some(PdfObject::Array(arr)) = self.operand_stack.pop() {
                    let array: Vec<f32> = arr
                        .iter()
                        .filter_map(|o| o.as_f64().ok().map(|v| v as f32))
                        .collect();
                    if array.is_empty() {
                        self.current.dash = None;
                    } else {
                        self.current.dash = Some(DashPattern { array, phase });
                    }
                }
            }
            "ri" => {
                // Rendering intent: a name operand selecting the colour intent.
                let name = self.pop_name();
                self.current.rendering_intent = zpdf_color::RenderIntent::from_pdf_name(&name);
            }
            "i" => {
                // Flatness tolerance — does not affect raster output.
            }
            "gs" => {
                let name = self.pop_name();
                self.apply_ext_gstate(&name);
            }

            // -- Path construction --
            "m" => {
                if let Some([x, y]) = self.path_operands() {
                    self.current_path.move_to(Point::new(x, y));
                }
            }
            "l" => {
                if let Some([x, y]) = self.path_operands() {
                    self.current_path.line_to(Point::new(x, y));
                }
            }
            "c" => {
                if let Some([x1, y1, x2, y2, x3, y3]) = self.path_operands() {
                    self.current_path.curve_to(
                        Point::new(x1, y1),
                        Point::new(x2, y2),
                        Point::new(x3, y3),
                    );
                }
            }
            "v" => {
                if let Some([x2, y2, x3, y3]) = self.path_operands() {
                    // v: current point is first control point
                    self.current_path.curve_to(
                        self.current_point(),
                        Point::new(x2, y2),
                        Point::new(x3, y3),
                    );
                }
            }
            "y" => {
                if let Some([x1, y1, x3, y3]) = self.path_operands() {
                    // y: end point is second control point
                    self.current_path.curve_to(
                        Point::new(x1, y1),
                        Point::new(x3, y3),
                        Point::new(x3, y3),
                    );
                }
            }
            "h" => self.current_path.close(),
            "re" => {
                if let Some([x, y, w, h]) = self.path_operands() {
                    self.current_path.rect(Rect::new(x, y, x + w, y + h));
                }
            }

            // -- Path painting -- (each then applies a pending `W` clip)
            "S" => {
                self.paint_stroke();
                self.flush_pending_clip();
            }
            "s" => {
                self.current_path.close();
                self.paint_stroke();
                self.flush_pending_clip();
            }
            "f" | "F" => {
                self.paint_fill(FillRule::NonZero);
                self.flush_pending_clip();
            }
            "f*" => {
                self.paint_fill(FillRule::EvenOdd);
                self.flush_pending_clip();
            }
            "B" => {
                self.paint_fill_then_stroke(FillRule::NonZero);
                self.flush_pending_clip();
            }
            "B*" => {
                self.paint_fill_then_stroke(FillRule::EvenOdd);
                self.flush_pending_clip();
            }
            "b" => {
                self.current_path.close();
                self.paint_fill_then_stroke(FillRule::NonZero);
                self.flush_pending_clip();
            }
            "b*" => {
                self.current_path.close();
                self.paint_fill_then_stroke(FillRule::EvenOdd);
                self.flush_pending_clip();
            }
            "n" => {
                self.current_path.clear();
                self.flush_pending_clip();
            }

            // -- Clipping --
            "W" => {
                let path = self.transform_path_to_page_space(&self.current_path);
                self.pending_clip = Some((path, FillRule::NonZero));
            }
            "W*" => {
                let path = self.transform_path_to_page_space(&self.current_path);
                self.pending_clip = Some((path, FillRule::EvenOdd));
            }

            // -- Color --
            "g" => {
                let gray = self.pop_f64() as f32;
                self.current.fill_color = Color::gray(gray);
                self.current.fill_cs = ActiveColorSpace::DeviceGray;
                self.current.fill_pattern = None;
                self.current.fill_colorant = Some(gray_colorant(gray));
                self.current.fill_color_explicit = true;
            }
            "G" => {
                let gray = self.pop_f64() as f32;
                self.current.stroke_color = Color::gray(gray);
                self.current.stroke_cs = ActiveColorSpace::DeviceGray;
                self.current.stroke_pattern = None;
                self.current.stroke_colorant = Some(gray_colorant(gray));
                self.current.stroke_color_explicit = true;
            }
            "rg" => {
                let b = self.pop_f64() as f32;
                let g = self.pop_f64() as f32;
                let r = self.pop_f64() as f32;
                self.current.fill_color = Color::rgb(r, g, b);
                self.current.fill_cs = ActiveColorSpace::DeviceRGB;
                self.current.fill_pattern = None;
                self.current.fill_colorant = None;
                self.current.fill_color_explicit = true;
            }
            "RG" => {
                let b = self.pop_f64() as f32;
                let g = self.pop_f64() as f32;
                let r = self.pop_f64() as f32;
                self.current.stroke_color = Color::rgb(r, g, b);
                self.current.stroke_cs = ActiveColorSpace::DeviceRGB;
                self.current.stroke_pattern = None;
                self.current.stroke_colorant = None;
                self.current.stroke_color_explicit = true;
            }
            "k" => {
                let k_val = self.pop_f64();
                let y_val = self.pop_f64();
                let m_val = self.pop_f64();
                let c_val = self.pop_f64();
                let (r, g, b) = self.cmyk_to_display(c_val, m_val, y_val, k_val);
                self.current.fill_color = Color::rgb(r as f32, g as f32, b as f32);
                self.current.fill_cs = ActiveColorSpace::DeviceCMYK;
                self.current.fill_pattern = None;
                self.current.fill_colorant = Some(cmyk_colorant(c_val, m_val, y_val, k_val));
                self.current.fill_color_explicit = true;
            }
            "K" => {
                let k_val = self.pop_f64();
                let y_val = self.pop_f64();
                let m_val = self.pop_f64();
                let c_val = self.pop_f64();
                let (r, g, b) = self.cmyk_to_display(c_val, m_val, y_val, k_val);
                self.current.stroke_color = Color::rgb(r as f32, g as f32, b as f32);
                self.current.stroke_cs = ActiveColorSpace::DeviceCMYK;
                self.current.stroke_pattern = None;
                self.current.stroke_colorant = Some(cmyk_colorant(c_val, m_val, y_val, k_val));
                self.current.stroke_color_explicit = true;
            }
            "cs" => {
                let name = self.pop_name();
                self.current.fill_cs = self.resolve_color_space(&name);
                self.current.fill_pattern = None;
                // Per spec, cs resets the color to the space's initial value
                // (black / index 0 / tint 1.0 all paint-as-dark; Pattern has none).
                if !matches!(self.current.fill_cs, ActiveColorSpace::Pattern { .. }) {
                    self.current.fill_color = self.initial_color(&self.current.fill_cs);
                    let cs = self.current.fill_cs.clone();
                    self.current.fill_colorant = self.initial_colorant(&cs);
                    self.current.fill_color_explicit = true;
                } else {
                    // Pattern is not a colorant space; the colour (and overprint
                    // colorant) arrive with scn. Clear stale colorant meanwhile.
                    self.current.fill_colorant = None;
                }
            }
            "CS" => {
                let name = self.pop_name();
                self.current.stroke_cs = self.resolve_color_space(&name);
                self.current.stroke_pattern = None;
                if !matches!(self.current.stroke_cs, ActiveColorSpace::Pattern { .. }) {
                    self.current.stroke_color = self.initial_color(&self.current.stroke_cs);
                    let cs = self.current.stroke_cs.clone();
                    self.current.stroke_colorant = self.initial_colorant(&cs);
                    self.current.stroke_color_explicit = true;
                } else {
                    self.current.stroke_colorant = None;
                }
            }
            "sc" | "scn" => {
                if let ActiveColorSpace::Pattern { base } = self.current.fill_cs.clone() {
                    let name = self.pop_name();
                    let (pattern, mut approx) = self.resolve_pattern(&name);
                    // Uncolored tiling pattern: the operands before the name
                    // are the cell color in the pattern's base space.
                    if let Some(c) = self.uncolored_pattern_color(&pattern, base.as_deref()) {
                        approx = c;
                    }
                    self.current.fill_pattern = pattern;
                    self.current.fill_color = approx;
                    self.current.fill_colorant = None;
                } else {
                    let cs = self.current.fill_cs.clone();
                    let (color, colorant) = self.pop_color_and_colorant(&cs);
                    self.current.fill_color = color;
                    self.current.fill_colorant = colorant;
                }
                self.current.fill_color_explicit = true;
            }
            "SC" | "SCN" => {
                if let ActiveColorSpace::Pattern { base } = self.current.stroke_cs.clone() {
                    let name = self.pop_name();
                    let (pattern, mut approx) = self.resolve_pattern(&name);
                    if let Some(c) = self.uncolored_pattern_color(&pattern, base.as_deref()) {
                        approx = c;
                    }
                    self.current.stroke_pattern = pattern;
                    self.current.stroke_color = approx;
                    self.current.stroke_colorant = None;
                } else {
                    let cs = self.current.stroke_cs.clone();
                    let (color, colorant) = self.pop_color_and_colorant(&cs);
                    self.current.stroke_color = color;
                    self.current.stroke_colorant = colorant;
                }
                self.current.stroke_color_explicit = true;
            }

            // -- Shading --
            "sh" => {
                let name = self.pop_name();
                self.paint_shading_op(&name);
            }

            // -- Text --
            "BT" => {
                self.text_active = true;
                self.text_matrix = Matrix::identity();
                self.text_line_matrix = Matrix::identity();
                self.text_clip_accum = None;
                self.text_clip_requested = false;
                self.text_clip_unavailable = false;
            }
            "ET" => {
                self.text_active = false;
                self.apply_text_clip();
            }
            "Tf" => {
                let size = self.pop_f64() as f32;
                let name = self.pop_name();
                self.current.font_name = name.clone();
                self.current.font_size = size;
                // An unresolvable font must not leave the previous one active.
                self.current_font_id = None;
                let lookup_name = self.resolve_font_name(&name);
                if let Some(fc) = self.font_cache.as_ref() {
                    if let Some((fid, _font)) = fc.get_by_name(&lookup_name) {
                        self.current_font_id = Some(fid);
                    }
                }
            }
            "Td" => {
                let ty = self.pop_f64();
                let tx = self.pop_f64();
                let translate = Matrix::translate(tx, ty);
                self.text_line_matrix = self.text_line_matrix.concat(&translate);
                self.text_matrix = self.text_line_matrix;
            }
            "TD" => {
                let ty = self.pop_f64();
                let tx = self.pop_f64();
                self.current.leading = -ty as f32;
                let translate = Matrix::translate(tx, ty);
                self.text_line_matrix = self.text_line_matrix.concat(&translate);
                self.text_matrix = self.text_line_matrix;
            }
            "Tm" => {
                let f = self.pop_f64();
                let e = self.pop_f64();
                let d = self.pop_f64();
                let c = self.pop_f64();
                let b = self.pop_f64();
                let a = self.pop_f64();
                let m = Matrix::new(a, b, c, d, e, f);
                self.text_matrix = m;
                self.text_line_matrix = m;
            }
            "T*" => {
                let leading = self.current.leading as f64;
                let translate = Matrix::translate(0.0, -leading);
                self.text_line_matrix = self.text_line_matrix.concat(&translate);
                self.text_matrix = self.text_line_matrix;
            }
            "Tc" => self.current.char_spacing = self.pop_f64() as f32,
            "Tw" => self.current.word_spacing = self.pop_f64() as f32,
            "Tz" => self.current.h_scaling = self.pop_f64() as f32,
            "TL" => self.current.leading = self.pop_f64() as f32,
            "Ts" => self.current.rise = self.pop_f64() as f32,
            "Tr" => self.current.render_mode = self.pop_f64() as u8,
            "Tj" => {
                let bytes = self.pop_string_bytes();
                let restore = self.leaked_font_override(|| std::iter::once(bytes.as_slice()));
                self.show_text(&bytes);
                self.restore_font(restore);
            }
            "TJ" => {
                if let Some(PdfObject::Array(arr)) = self.operand_stack.pop() {
                    // A leaked font is judged for the whole array before any
                    // element is shown.
                    let restore = self.leaked_font_override(|| {
                        arr.iter().filter_map(|item| match item {
                            PdfObject::String(s) => Some(s.0.as_slice()),
                            _ => None,
                        })
                    });
                    for (index, item) in arr.into_iter().enumerate() {
                        if self.timed_out || self.resource_limit_hit {
                            break;
                        }
                        if text_deadline_reached(index, self.deadline) {
                            self.timed_out = true;
                            break;
                        }
                        match item {
                            PdfObject::String(s) => self.show_text(&s.0),
                            PdfObject::Integer(n) => {
                                self.adjust_text_position(-(n as f64));
                            }
                            PdfObject::Real(n) => {
                                self.adjust_text_position(-n);
                            }
                            _ => {}
                        }
                    }
                    self.restore_font(restore);
                }
            }
            "'" => {
                // Move to next line and show text
                let leading = self.current.leading as f64;
                let translate = Matrix::translate(0.0, -leading);
                self.text_line_matrix = self.text_line_matrix.concat(&translate);
                self.text_matrix = self.text_line_matrix;
                let bytes = self.pop_string_bytes();
                let restore = self.leaked_font_override(|| std::iter::once(bytes.as_slice()));
                self.show_text(&bytes);
                self.restore_font(restore);
            }
            "\"" => {
                let bytes = self.pop_string_bytes();
                let char_spacing = self.pop_f64() as f32;
                let word_spacing = self.pop_f64() as f32;
                self.current.word_spacing = word_spacing;
                self.current.char_spacing = char_spacing;
                let leading = self.current.leading as f64;
                let translate = Matrix::translate(0.0, -leading);
                self.text_line_matrix = self.text_line_matrix.concat(&translate);
                self.text_matrix = self.text_line_matrix;
                let restore = self.leaked_font_override(|| std::iter::once(bytes.as_slice()));
                self.show_text(&bytes);
                self.restore_font(restore);
            }

            // -- XObject --
            "Do" => {
                let name = self.pop_name();
                self.execute_do(&name);
            }

            // -- Type3 glyph operators (ISO 32000-1 §9.6.5.4) --
            "d0" => {
                // wx wy d0: set glyph width; colour operators remain enabled
                // unless an enclosing uncoloured pattern or `d1` glyph already
                // locked colour.
                let _wy = self.pop_f64();
                let _wx = self.pop_f64();
                if self.type3_glyph {
                    self.suppress_color_ops = false;
                }
            }
            "d1" => {
                // wx wy llx lly urx ury d1: set glyph width and bbox, and lock
                // colours to the enclosing non-stroking (fill) paint. Per
                // ISO 32000-1 Table 113, `d1` is only valid in a Type 3
                // CharProc stream; ignore misplaced `d1` in page, Form, or
                // pattern streams so it cannot lock outer colour operators.
                let _ury = self.pop_f64();
                let _urx = self.pop_f64();
                let _lly = self.pop_f64();
                let _llx = self.pop_f64();
                let _wy = self.pop_f64();
                let _wx = self.pop_f64();
                if self.type3_inline_ctx.is_some() {
                    self.suppress_color_ops = true;
                    self.current.stroke_color = self.current.fill_color;
                    self.current.stroke_alpha = self.current.fill_alpha;
                    self.current.stroke_cs = self.current.fill_cs.clone();
                    self.current.stroke_pattern = self.current.fill_pattern.clone();
                    self.current.stroke_overprint = self.current.fill_overprint;
                    self.current.stroke_colorant = self.current.fill_colorant;
                } else if self.type3_glyph {
                    self.suppress_color_ops = true;
                    self.current.fill_color_explicit = false;
                    self.current.stroke_color_explicit = false;
                }
            }

            // -- Marked content --
            "BMC" => {
                if self.mc_stack.len() >= self.max_marked_content_depth {
                    tracing::warn!(
                        "marked-content stack exceeded {} entries; truncating page",
                        self.max_marked_content_depth
                    );
                    self.resource_limit_hit = true;
                    return;
                }
                self.mc_depth += 1;
                // A plain marked-content sequence declares no /MCID.
                self.mc_stack.push(MarkedContent::default());
            }
            "BDC" => {
                if self.mc_stack.len() >= self.max_marked_content_depth {
                    tracing::warn!(
                        "marked-content stack exceeded {} entries; truncating page",
                        self.max_marked_content_depth
                    );
                    self.resource_limit_hit = true;
                    return;
                }
                // Operands: tag (first) then properties (top of stack).
                let props = self.operand_stack.pop();
                let tag = match self.operand_stack.pop() {
                    Some(PdfObject::Name(n)) => n.0,
                    _ => String::new(),
                };
                self.mc_depth += 1;
                // Capture this sequence's /MCID (if any) for text extraction; a
                // sequence with no /MCID inherits the enclosing one via the stack.
                let mcid = props.as_ref().and_then(|p| self.marked_content_id(p));
                if tag == "OC" && self.oc_hidden_from.is_none() {
                    let visible = props
                        .as_ref()
                        .map(|p| self.oc_properties_visible(p))
                        .unwrap_or(true);
                    if !visible {
                        self.oc_hidden_from = Some(self.mc_depth);
                    }
                }
                let actual = props.as_ref().and_then(|p| self.begin_actual_text(p, mcid));
                let ends_actual_text = actual.is_some();
                if let Some(actual) = actual {
                    self.active_actual_text = Some(actual);
                }
                self.mc_stack.push(MarkedContent {
                    mcid,
                    ends_actual_text,
                });
            }
            "EMC" => self.end_marked_content(true),
            "MP" | "DP" => {}
            "BX" => self.compatibility_depth = self.compatibility_depth.saturating_add(1),
            "EX" => self.compatibility_depth = self.compatibility_depth.saturating_sub(1),
            _ if self.compatibility_depth == 0 => {
                self.stats.diagnostics.unsupported_features = self
                    .stats
                    .diagnostics
                    .unsupported_features
                    .saturating_add(1);
            }
            _ => {}
        }
    }

    fn resolve_color_space(&mut self, name: &str) -> ActiveColorSpace {
        self.resolve_color_space_depth(name, 0)
    }

    /// [`Self::resolve_color_space`] `depth` levels into a colour space
    /// definition. A resource name met again inside a definition (`/CS0 /CS0`,
    /// or an Indexed base naming its own resource) keeps counting, so a cycle
    /// ends at the depth limit instead of overflowing the stack.
    fn resolve_color_space_depth(&mut self, name: &str, depth: u8) -> ActiveColorSpace {
        match name {
            "DeviceGray" | "G" => ActiveColorSpace::DeviceGray,
            "DeviceRGB" | "RGB" => ActiveColorSpace::DeviceRGB,
            "DeviceCMYK" | "CMYK" => ActiveColorSpace::DeviceCMYK,
            "Pattern" => ActiveColorSpace::Pattern { base: None },
            _ => {
                // Check both direct and indirect entries within each scope
                // before advancing to its parent; indirect refs pass through as
                // `PdfObject::Ref` so `cs_cache` builds each space once.
                let Some(obj) = self.lookup_color_space_resource(name) else {
                    return ActiveColorSpace::DeviceGray;
                };
                self.resolve_color_space_obj(&obj, depth)
            }
        }
    }

    /// Resolve a colorspace object (name or array) into an evaluatable space.
    fn resolve_color_space_obj(&mut self, obj: &PdfObject, depth: u8) -> ActiveColorSpace {
        if depth > 4 {
            return ActiveColorSpace::DeviceGray;
        }

        // Check cache for indirect refs (common: same ColorSpace object reused)
        let cache_key = if let PdfObject::Ref(r) = obj {
            if let Some(cached) = self.cs_cache.borrow().get(r) {
                return cached.clone();
            }
            Some(*r)
        } else {
            None
        };

        let resolved;
        let obj = if let PdfObject::Ref(r) = obj {
            match self.file.map(|f| f.resolve(*r)) {
                Some(Ok(o)) => {
                    resolved = o;
                    &resolved
                }
                _ => return ActiveColorSpace::DeviceGray,
            }
        } else {
            obj
        };
        let arr = match obj {
            PdfObject::Name(n) => return self.resolve_color_space_depth(&n.0, depth + 1),
            PdfObject::Array(arr) => arr,
            _ => return ActiveColorSpace::DeviceGray,
        };
        let Some(PdfObject::Name(cs_name)) = arr.first() else {
            return ActiveColorSpace::DeviceGray;
        };
        let result = match cs_name.as_str() {
            "ICCBased" => {
                let n = arr
                    .get(1)
                    .and_then(|o| self.resolve_dict_of(o))
                    .and_then(|d| d.get_i64("N").ok())
                    .unwrap_or(3);
                if let Some(t) = self.resolve_icc_transform(arr.get(1), n) {
                    return ActiveColorSpace::Icc(t);
                }
                match n {
                    1 => ActiveColorSpace::DeviceGray,
                    4 => ActiveColorSpace::DeviceCMYK,
                    3 => ActiveColorSpace::DeviceRGB,
                    other => ActiveColorSpace::ICCBased(other.clamp(1, 32) as u8),
                }
            }
            "Separation" | "DeviceN" => {
                let is_sep = cs_name.as_str() == "Separation";
                // Colorant names: Separation carries a single Name at [1];
                // DeviceN/NChannel an array of Names. An unreadable entry becomes
                // "" (never a process/None/All match → ordinary spot polarity).
                let names: Vec<String> = if is_sep {
                    match arr.get(1).map(|o| self.resolve_plain(o)) {
                        Some(PdfObject::Name(nm)) => vec![nm.0],
                        _ => vec![String::new()],
                    }
                } else {
                    match arr.get(1).map(|o| self.resolve_plain(o)) {
                        Some(PdfObject::Array(items)) => items
                            .iter()
                            .map(|o| match self.resolve_plain(o) {
                                PdfObject::Name(nm) => nm.0,
                                _ => String::new(),
                            })
                            .collect(),
                        _ => Vec::new(),
                    }
                };
                let names = if names.is_empty() {
                    vec![String::new()]
                } else {
                    names
                };
                let n = names.len();
                let alternate = arr
                    .get(2)
                    .map(|o| self.resolve_color_space_obj(o, depth + 1))
                    .unwrap_or(ActiveColorSpace::DeviceGray);
                let transform = arr.get(3).and_then(|o| self.parse_function(o));
                // PDF 2.0 NChannel: an attributes dict at [4] may carry
                // `/Colorants` (each spot colorant's own Separation space),
                // enabling a precise per-colorant overprint mask. DeviceN only —
                // a Separation has a single colorant already handled directly.
                let colorants = if is_sep {
                    None
                } else {
                    self.parse_nchannel_colorants(&names, arr.get(4), depth)
                };
                ActiveColorSpace::Tint {
                    n,
                    transform: transform.map(std::sync::Arc::new),
                    alternate: Box::new(alternate),
                    names: std::sync::Arc::from(names),
                    colorants,
                }
            }
            "Indexed" | "I" => {
                let base = arr
                    .get(1)
                    .map(|o| self.resolve_color_space_obj(o, depth + 1))
                    .unwrap_or(ActiveColorSpace::DeviceRGB);
                let hival = arr
                    .get(2)
                    .and_then(|o| o.as_i64().ok())
                    .unwrap_or(255)
                    .clamp(0, 255) as u8;
                let lookup: Option<Vec<u8>> = match arr.get(3).map(|o| self.resolve_plain(o)) {
                    Some(PdfObject::String(s)) => Some(s.0.clone()),
                    Some(PdfObject::Ref(r)) => {
                        self.file.and_then(|f| f.resolve_stream_data(r).ok())
                    }
                    _ => None,
                };
                match lookup {
                    Some(lookup) if !lookup.is_empty() => {
                        // Bake an ICC base into the palette here (one buffer
                        // transform) so per-colour lookups stay device-RGB.
                        let (base, lookup) = match base {
                            ActiveColorSpace::Icc(t) => {
                                (ActiveColorSpace::DeviceRGB, t.palette_to_rgb(&lookup))
                            }
                            other => (other, lookup),
                        };
                        ActiveColorSpace::Indexed {
                            base: Box::new(base),
                            hival,
                            lookup: std::sync::Arc::from(lookup),
                        }
                    }
                    _ => ActiveColorSpace::DeviceGray,
                }
            }
            "Lab" => {
                let params = arr.get(1).and_then(|o| self.resolve_dict_of(o));
                let mut white_point = [0.9505, 1.0, 1.089];
                let mut range = [-100.0, 100.0, -100.0, 100.0];
                if let Some(d) = &params {
                    if let Ok(wp) = d.get_array("WhitePoint") {
                        for (i, v) in wp.iter().take(3).enumerate() {
                            if let Ok(x) = v.as_f64() {
                                white_point[i] = x;
                            }
                        }
                    }
                    if let Ok(rg) = d.get_array("Range") {
                        for (i, v) in rg.iter().take(4).enumerate() {
                            if let Some(x) = v.as_f64().ok().filter(|x| x.is_finite()) {
                                range[i] = x;
                            }
                        }
                    }
                }
                // Every use clamps into these pairs, which panics on an
                // inverted (`/Range [10 -10 …]`) or NaN one; order them.
                for pair in range.chunks_mut(2) {
                    if pair[0] > pair[1] {
                        pair.swap(0, 1);
                    }
                }
                ActiveColorSpace::Lab { white_point, range }
            }
            "CalGray" | "CalRGB" => {
                let params = arr
                    .get(1)
                    .and_then(|o| self.resolve_dict_of(o))
                    .unwrap_or_default();
                let mut resolve = |id| self.file.and_then(|f| f.resolve(id).ok());
                if cs_name.as_str() == "CalGray" {
                    ActiveColorSpace::CalGray(zpdf_color::CalGrayParams::from_pdf_dict(
                        &params,
                        &mut resolve,
                    ))
                } else {
                    ActiveColorSpace::CalRGB(zpdf_color::CalRGBParams::from_pdf_dict(
                        &params,
                        &mut resolve,
                    ))
                }
            }
            // `[/Pattern base]` — retain the base space for uncolored patterns.
            "Pattern" => ActiveColorSpace::Pattern {
                base: arr
                    .get(1)
                    .map(|o| Box::new(self.resolve_color_space_obj(o, depth + 1))),
            },
            _ => ActiveColorSpace::DeviceGray,
        };

        // Cache the result if it came from an indirect reference
        if let Some(key) = cache_key {
            self.cs_cache.borrow_mut().insert(key, result.clone());
        }
        result
    }

    /// Parse a DeviceN/NChannel attributes dict's `/Colorants` into per-input-
    /// colorant maps parallel to `names`, for the overprint active-colorant mask
    /// (PDF 8.6.7). Returns `Some` only when **every** colorant is classifiable
    /// — a `/None`, a standard process colorant, or a spot whose own Separation
    /// space is present in `/Colorants` — so a partial/absent `/Colorants` leaves
    /// overprint on the whole-transform projection (plain DeviceN unchanged).
    fn parse_nchannel_colorants(
        &mut self,
        names: &[String],
        attrs_obj: Option<&PdfObject>,
        depth: u8,
    ) -> Option<std::sync::Arc<[ColorantMap]>> {
        // `/Colorants` (NChannel attributes) maps each spot colorant name to its
        // own Separation colour space; absent on a plain DeviceN.
        let colorants_dict = attrs_obj
            .and_then(|o| self.resolve_dict_of(o))
            .and_then(|attrs| attrs.get("Colorants").cloned())
            .map(|o| self.resolve_plain(&o))
            .and_then(|o| o.as_dict().ok().cloned());

        // Only build a per-colorant mask when it can differ from the whole-
        // transform projection: an explicit `/Colorants` (NChannel) is present,
        // or a `/None` colorant must be excluded from the mask. A plain DeviceN
        // keeps the existing whole-transform overprint path byte-for-byte.
        let has_none = names.iter().any(|n| n == "None");
        if colorants_dict.is_none() && !has_none {
            return None;
        }

        let mut maps = Vec::with_capacity(names.len());
        for name in names {
            if name == "None" {
                maps.push(ColorantMap::None);
            } else if let Some(ch) = process_colorant_index(name) {
                maps.push(ColorantMap::Process(ch));
            } else {
                // A spot colorant: classify via its `/Colorants` Separation space
                // when present.
                let sep = colorants_dict
                    .as_ref()
                    .and_then(|d| d.get(name.as_str()))
                    .map(|o| self.resolve_color_space_obj(o, depth + 1));
                match sep {
                    Some(space @ ActiveColorSpace::Tint { .. }) => {
                        maps.push(ColorantMap::Spot(Box::new(space)));
                    }
                    // Unclassifiable spot. Only keep the per-colorant map when a
                    // `/None` must be excluded (then isolate this spot through the
                    // whole tint transform); with no `/None` to drop, fall back
                    // entirely to the faithful whole-transform projection so a
                    // plain DeviceN loses no precision.
                    _ if has_none => maps.push(ColorantMap::Whole),
                    _ => return None,
                }
            }
        }
        Some(std::sync::Arc::from(maps))
    }

    /// Compile (through the document's `IccCache`) the ICCBased profile
    /// stream `obj` into a transform. Any failure — no cache injected, an
    /// unresolvable stream, a malformed/unsupported profile, or a profile
    /// whose channel count contradicts `/N` — yields `None` so the caller
    /// keeps the component-count fallback.
    fn resolve_icc_transform(
        &mut self,
        obj: Option<&PdfObject>,
        n: i64,
    ) -> Option<std::sync::Arc<zpdf_color::IccTransform>> {
        self.icc_cache.as_ref()?;
        let file = self.file;
        let intent = self.current.rendering_intent;
        let transform = match obj? {
            PdfObject::Ref(r) => {
                let file = file?;
                let cache = self.icc_cache.as_deref_mut()?;
                cache.get_or_build(*r, intent, || file.resolve_stream_data(*r).ok())
            }
            // Inline profile streams (synthetic content; the spec requires an
            // indirect stream) have no object id to cache under.
            PdfObject::Stream(s) => build_inline_icc_transform(file, s, intent),
            _ => None,
        }?;
        if transform.components() != n.max(1) as usize {
            tracing::warn!(
                "ICC profile has {} components but /N is {n}; using /N fallback",
                transform.components()
            );
            return None;
        }
        Some(transform)
    }

    /// Resolve one level of indirection, returning the object itself otherwise.
    fn resolve_plain(&self, obj: &PdfObject) -> PdfObject {
        match obj {
            PdfObject::Ref(r) => match self.file.map(|f| f.resolve(*r)) {
                Some(Ok(o)) => o,
                _ => PdfObject::Null,
            },
            other => other.clone(),
        }
    }

    /// Dict view of an object that may be a dict, a stream, or a ref to either.
    fn resolve_dict_of(&self, obj: &PdfObject) -> Option<zpdf_core::PdfDict> {
        let o = self.resolve_plain(obj);
        if let Ok(d) = o.as_dict() {
            return Some(d.clone());
        }
        if let Ok(s) = o.as_stream() {
            return Some(s.dict.clone());
        }
        None
    }

    /// Parse a PDF function object (with stream data when applicable).
    fn parse_function(&self, obj: &PdfObject) -> Option<zpdf_color::PdfFunction> {
        let file = self.file?;
        let mut resolve = |id: zpdf_core::ObjectId| {
            let o = file.resolve(id).ok()?;
            let data = if o.as_stream().is_ok() {
                file.resolve_stream_data(id).ok()
            } else {
                None
            };
            // Streams must hand back their dict, not the stream object.
            let obj = match &o {
                PdfObject::Stream(s) => PdfObject::Dict(s.dict.clone()),
                other => other.clone(),
            };
            Some((obj, data))
        };
        zpdf_color::PdfFunction::parse_object(obj, &mut resolve)
    }

    /// Initial color for a freshly-selected colorspace (per PDF 8.6.8: all
    /// device/CIE components 0; Indexed index 0; tint 1.0 for colorants).
    fn initial_color(&self, cs: &ActiveColorSpace) -> Color {
        match cs {
            ActiveColorSpace::Indexed { .. } => self.components_to_rgb(cs, &[0.0]),
            ActiveColorSpace::Tint { .. } => self.components_to_rgb(cs, &[1.0]),
            ActiveColorSpace::Lab { .. } => self.components_to_rgb(cs, &[0.0, 0.0, 0.0]),
            _ => Color::black(),
        }
    }

    /// Colorant projection for a freshly-selected space at its initial value
    /// (mirrors [`Self::initial_color`]: device/CIE components 0; tint 1.0).
    fn initial_colorant(&self, cs: &ActiveColorSpace) -> Option<Colorant> {
        // Mirror `initial_color` (which is black for every device/ICC space): a
        // CMYK/ICC-4 space starts at K=1, gray/ICC-1 at black. A compiled ICC
        // profile (`Icc`) is the common ICCBased case — handle it, not just the
        // profileless `ICCBased(n)` fallback, so the colorant agrees with the
        // displayed initial colour instead of projecting to all-zeros (white).
        let vals: &[f64] = match cs {
            ActiveColorSpace::Indexed { .. } => &[0.0],
            ActiveColorSpace::Tint { .. } => &[1.0],
            ActiveColorSpace::DeviceCMYK | ActiveColorSpace::ICCBased(4) => &[0.0, 0.0, 0.0, 1.0],
            ActiveColorSpace::Icc(t) if t.components() == 4 => &[0.0, 0.0, 0.0, 1.0],
            ActiveColorSpace::DeviceGray | ActiveColorSpace::ICCBased(1) => &[0.0],
            ActiveColorSpace::Icc(t) if t.components() == 1 => &[0.0],
            _ => &[0.0, 0.0, 0.0, 0.0],
        };
        self.colorant_projection(cs, vals)
    }

    fn pop_color(&mut self, cs: &ActiveColorSpace) -> Color {
        let n = cs.components();
        let mut vals = Vec::with_capacity(n);
        for _ in 0..n {
            vals.push(self.pop_f64());
        }
        vals.reverse();
        self.components_to_rgb(cs, &vals)
    }

    /// Pop `cs`'s components once, returning both the display RGB and its CMYK
    /// colorant projection (for overprint). Avoids popping the operands twice.
    fn pop_color_and_colorant(&mut self, cs: &ActiveColorSpace) -> (Color, Option<Colorant>) {
        let n = cs.components();
        let mut vals = Vec::with_capacity(n);
        for _ in 0..n {
            vals.push(self.pop_f64());
        }
        vals.reverse();
        (
            self.components_to_rgb(cs, &vals),
            self.colorant_projection(cs, &vals),
        )
    }

    /// DeviceCMYK → sRGB for display. Routes through the document's output-intent
    /// default CMYK profile (the PDF/X characterized printing condition) when one
    /// is active, otherwise the Adobe SWOP polynomial. This is the single gate
    /// for the `k`/`K` operators and the `DeviceCMYK | ICCBased(4)` arm of
    /// [`Self::components_to_rgb`] — an explicitly compiled embedded ICCBased(4)
    /// profile is the `Icc` variant and never reaches here, so its own profile
    /// always wins; the output intent only substitutes for raw DeviceCMYK.
    fn cmyk_to_display(&self, c: f64, m: f64, y: f64, k: f64) -> (f64, f64, f64) {
        let cmyk = [c, m, y, k];
        // Check cache for repeated identical CMYK values (common in technical drawings)
        if let Some((cached_cmyk, cached_rgb)) = *self.cmyk_cache.borrow() {
            if cached_cmyk == cmyk {
                return cached_rgb;
            }
        }
        // Compute and cache the result
        let rgb = match &self.output_intent_cmyk {
            Some(t) => t.color_to_rgb(&cmyk),
            None => zpdf_color::cmyk_to_rgb(c, m, y, k),
        };
        *self.cmyk_cache.borrow_mut() = Some((cmyk, rgb));
        rgb
    }

    /// Re-route a DeviceCMYK image colour space through the active output-intent
    /// profile, mirroring [`Self::cmyk_to_display`] for raster pixels. A bare
    /// `Cmyk` space becomes the profile's `Icc` space (so the decoder colour-
    /// manages every pixel); an `Indexed` space with a DeviceCMYK base has its
    /// palette baked to RGB through the profile (the same one-buffer transform
    /// `resolve_image_colorspace` applies to embedded-ICC bases); a
    /// Separation/DeviceN alternate is re-routed the same way. No-op when no
    /// output intent is active, leaving the existing SWOP path untouched.
    fn colormanage_cmyk_image(
        &self,
        cs: Option<zpdf_image::ResolvedColorSpace>,
    ) -> Option<zpdf_image::ResolvedColorSpace> {
        use zpdf_image::ResolvedColorSpace as Rcs;
        fn route(cs: Rcs, oi: &std::sync::Arc<zpdf_color::IccTransform>) -> Rcs {
            match cs {
                Rcs::Cmyk => Rcs::Icc {
                    ncomp: 4,
                    transform: oi.clone(),
                },
                Rcs::Indexed {
                    base,
                    hival,
                    lookup,
                } if matches!(*base, Rcs::Cmyk) => Rcs::Indexed {
                    base: Box::new(Rcs::Rgb),
                    hival,
                    lookup: oi.palette_to_rgb(&lookup),
                },
                Rcs::Indexed {
                    base,
                    hival,
                    lookup,
                } => Rcs::Indexed {
                    base: Box::new(route(*base, oi)),
                    hival,
                    lookup,
                },
                Rcs::Tint {
                    n,
                    transform,
                    alternate,
                } => Rcs::Tint {
                    n,
                    transform,
                    alternate: Box::new(route(*alternate, oi)),
                },
                other => other,
            }
        }
        let Some(oi) = &self.output_intent_cmyk else {
            return cs;
        };
        cs.map(|cs| route(cs, oi))
    }

    /// Convert component values in `cs` to an RGB display color.
    fn components_to_rgb(&self, cs: &ActiveColorSpace, vals: &[f64]) -> Color {
        let get = |i: usize| vals.get(i).copied().unwrap_or(0.0);
        match cs {
            ActiveColorSpace::CalGray(params) => {
                let (r, g, b) = params.to_rgb(get(0));
                Color::rgb(r as f32, g as f32, b as f32)
            }
            ActiveColorSpace::CalRGB(params) => {
                let (r, g, b) = params.to_rgb([get(0), get(1), get(2)]);
                Color::rgb(r as f32, g as f32, b as f32)
            }
            ActiveColorSpace::DeviceGray | ActiveColorSpace::ICCBased(1) => {
                Color::gray(get(0) as f32)
            }
            ActiveColorSpace::DeviceRGB | ActiveColorSpace::ICCBased(3) => {
                Color::rgb(get(0) as f32, get(1) as f32, get(2) as f32)
            }
            ActiveColorSpace::DeviceCMYK | ActiveColorSpace::ICCBased(4) => {
                let (r, g, b) = self.cmyk_to_display(get(0), get(1), get(2), get(3));
                Color::rgb(r as f32, g as f32, b as f32)
            }
            ActiveColorSpace::ICCBased(_) => Color::gray(get(0) as f32),
            ActiveColorSpace::Icc(transform) => {
                let (r, g, b) = transform.color_to_rgb(vals);
                Color::rgb(r as f32, g as f32, b as f32)
            }
            ActiveColorSpace::Lab { white_point, range } => {
                let l = get(0).clamp(0.0, 100.0);
                let a = get(1).clamp(range[0], range[1]);
                let b = get(2).clamp(range[2], range[3]);
                let (r, g, bb) = zpdf_color::lab_to_rgb(l, a, b, *white_point);
                Color::rgb(r as f32, g as f32, bb as f32)
            }
            ActiveColorSpace::Indexed {
                base,
                hival,
                lookup,
            } => {
                let idx = (get(0).round().max(0.0) as usize).min(*hival as usize);
                let bn = base.components();
                let start = idx * bn;
                let mut comps = Vec::with_capacity(bn);
                for i in 0..bn {
                    let byte = lookup.get(start + i).copied().unwrap_or(0);
                    // Component byte scaled to the base space's nominal range.
                    let v = byte as f64 / 255.0;
                    comps.push(match base.as_ref() {
                        ActiveColorSpace::Lab { range, .. } => {
                            // Lab components are not 0..1; scale per Decode default.
                            match i {
                                0 => v * 100.0,
                                1 => range[0] + v * (range[1] - range[0]),
                                _ => range[2] + v * (range[3] - range[2]),
                            }
                        }
                        _ => v,
                    });
                }
                self.components_to_rgb(base, &comps)
            }
            ActiveColorSpace::Tint {
                n,
                transform,
                alternate,
                ..
            } => {
                if let Some(f) = transform {
                    if let Some(out) = f.eval(vals) {
                        return self.components_to_rgb(alternate, &out);
                    }
                }
                // No usable transform: dark-for-full-tint approximation.
                let max_tint = vals
                    .iter()
                    .take(*n)
                    .fold(0.0f64, |acc, &v| acc.max(v.clamp(0.0, 1.0)));
                Color::gray(1.0 - max_tint as f32)
            }
            ActiveColorSpace::Pattern { .. } => Color::black(),
        }
    }

    /// CMYK colorant projection of `vals` in `cs` for overprint (PDF 8.6.7),
    /// or `None` for additive / CIE / indexed / pattern spaces where overprint
    /// has no visible effect (they paint all process colorants → knockout).
    fn colorant_projection(&self, cs: &ActiveColorSpace, vals: &[f64]) -> Option<Colorant> {
        let get = |i: usize| vals.get(i).copied().unwrap_or(0.0);
        match cs {
            ActiveColorSpace::DeviceGray | ActiveColorSpace::ICCBased(1) => {
                Some(gray_colorant(get(0) as f32))
            }
            ActiveColorSpace::DeviceCMYK | ActiveColorSpace::ICCBased(4) => {
                Some(cmyk_colorant(get(0), get(1), get(2), get(3)))
            }
            // ICC CMYK: treat the input components as CMYK tints (OPM applies).
            ActiveColorSpace::Icc(t) if t.components() == 4 => {
                Some(cmyk_colorant(get(0), get(1), get(2), get(3)))
            }
            ActiveColorSpace::Tint {
                n,
                transform,
                alternate,
                names,
                colorants,
            } => {
                // The Separation special colorant `All` marks *every* colorant
                // (registration marks, §8.6.6.4) — not a selective overprint, so
                // it knocks out: report no colorant projection (→ normal paint).
                if names.len() == 1 && names[0] == "All" {
                    return None;
                }
                // An all-`None` space produces no marks (the paint is suppressed
                // upstream); it touches no colorants either.
                if !names.is_empty() && names.iter().all(|s| s == "None") {
                    return None;
                }
                // NChannel `/Colorants`: project each input colorant on its own —
                // a `/None` colorant adds no ink, a process colorant sets its
                // channel, a spot runs its individual Separation transform, an
                // unclassifiable spot is isolated through the whole transform —
                // then union the nonzero process colorants. (The *display* colour
                // still uses the whole tint transform above; this is only the
                // overprint active-colorant mask, which the whole-transform
                // projection cannot express because it can't drop a `/None`.)
                if let Some(maps) = colorants {
                    let mut cmyk = [0.0f32; 4];
                    let union = |cmyk: &mut [f32; 4], src: [f32; 4]| {
                        for (dst, s) in cmyk.iter_mut().zip(src.iter()) {
                            *dst = dst.max(*s);
                        }
                    };
                    for (i, m) in maps.iter().enumerate() {
                        let tint = vals.get(i).copied().unwrap_or(0.0);
                        match m {
                            ColorantMap::None => {}
                            ColorantMap::Process(ch) => cmyk[*ch] = cmyk[*ch].max(fin(tint)),
                            ColorantMap::Spot(sep) => {
                                if let Some(c) = self.colorant_projection(sep, &[tint]) {
                                    union(&mut cmyk, c.cmyk);
                                }
                            }
                            ColorantMap::Whole => {
                                // Isolate this colorant by running the DeviceN's
                                // own tint transform with only its tint set, so a
                                // sibling `/None` contributes no ink.
                                let mut iso = vec![0.0f64; maps.len()];
                                if let Some(slot) = iso.get_mut(i) {
                                    *slot = tint;
                                }
                                union(
                                    &mut cmyk,
                                    self.tint_whole_colorant(
                                        transform.as_deref(),
                                        alternate,
                                        maps.len(),
                                        &iso,
                                    ),
                                );
                            }
                        }
                    }
                    return Some(Colorant {
                        cmyk,
                        cmyk_device: false,
                    });
                }
                // Whole-transform projection: map the tints through the transform
                // to the alternate space and recover that space's colorants. A
                // spot/DeviceN paints exactly what it maps to → nonzero rule.
                Some(Colorant {
                    cmyk: self.tint_whole_colorant(transform.as_deref(), alternate, *n, vals),
                    cmyk_device: false,
                })
            }
            // DeviceRGB, ICCBased(3)/odd, Icc(non-4), Lab, Indexed, Pattern:
            // additive or non-colorant — overprint is a no-op.
            _ => None,
        }
    }

    /// Whole-transform overprint projection for a Separation/DeviceN: map `vals`
    /// through the tint `transform` to `alternate` and recover that space's CMYK
    /// colorants. With no usable transform, falls back to K-only at the maximum
    /// tint (the right polarity for a colorant). `n` bounds the fallback's tint
    /// scan to the declared component count.
    fn tint_whole_colorant(
        &self,
        transform: Option<&zpdf_color::PdfFunction>,
        alternate: &ActiveColorSpace,
        n: usize,
        vals: &[f64],
    ) -> [f32; 4] {
        let alt_vals = match transform.and_then(|f| f.eval(vals)) {
            Some(out) => out,
            None => {
                // No usable transform: dark-for-full-tint ≈ K only.
                let max_tint = vals
                    .iter()
                    .take(n)
                    .fold(0.0f64, |acc, &v| acc.max(v.clamp(0.0, 1.0)));
                return [0.0, 0.0, 0.0, fin(max_tint)];
            }
        };
        match self.colorant_projection(alternate, &alt_vals) {
            Some(c) => c.cmyk,
            None => {
                // Alternate is additive/CIE: project its RGB to CMYK.
                let rgb = self.components_to_rgb(alternate, &alt_vals);
                let (c, m, y, k) =
                    zpdf_color::rgb_to_cmyk_naive(rgb.r as f64, rgb.g as f64, rgb.b as f64);
                [fin(c), fin(m), fin(y), fin(k)]
            }
        }
    }

    /// Build the display-list overprint descriptor for a paint, or `None` when
    /// overprint is off, the colour is not in a device-colorant space, or it
    /// paints all four process colorants (identical to a normal opaque paint,
    /// which renders through the fidelity-CMYK path instead of naïve CMYK).
    fn overprint_descriptor(&self, enabled: bool, colorant: Option<Colorant>) -> Option<Overprint> {
        if !enabled {
            return None;
        }
        let Colorant { cmyk, cmyk_device } = colorant?;
        // OPM only distinguishes all-vs-nonzero for a DeviceCMYK source; spot
        // and gray colorants always paint exactly their nonzero channels.
        let active: u8 = if cmyk_device && self.current.overprint_mode == 0 {
            0b1111
        } else {
            let mut m = 0u8;
            for (i, &v) in cmyk.iter().enumerate() {
                if v != 0.0 {
                    m |= 1 << i;
                }
            }
            m
        };
        if active == 0b1111 {
            None
        } else {
            Some(Overprint { cmyk, active })
        }
    }

    /// Resolve a pattern name selected via `scn`/`SCN`. Returns the pattern
    /// paint (if usable) plus a solid approximation color for paths that
    /// cannot take the real paint (e.g. shading-pattern strokes).
    fn resolve_pattern(&mut self, name: &str) -> (Option<PatternPaint>, Color) {
        let Some(file) = self.file else {
            return (None, Color::gray(0.5));
        };
        let Some((pat_id, scope_base_ctm, pat_scope)) =
            self.lookup_res_with_base_ctm(|r| r.patterns.get(name).copied())
        else {
            tracing::debug!("pattern {name} not found in resources");
            return (None, Color::gray(0.5));
        };
        self.record_tiling_inherited_pattern_scope(pat_scope);
        if let Some(def) = self.tiling_pattern_cache.get(&pat_id) {
            return (
                def.clone().map(|def| PatternPaint::Tiling {
                    def,
                    base_ctm: scope_base_ctm,
                }),
                Color::gray(0.5),
            );
        }
        let shading_key = (pat_id, matrix_bits(&scope_base_ctm));
        if let Some(built) = self.shading_pattern_cache.get(&shading_key) {
            return shading_pattern_paint(built.as_ref());
        }
        let Ok(obj) = file.resolve(pat_id) else {
            return (None, Color::gray(0.5));
        };
        let dict = match &obj {
            PdfObject::Stream(s) => &s.dict,
            PdfObject::Dict(d) => d,
            _ => return (None, Color::gray(0.5)),
        };
        let ptype = dict.get_i64("PatternType").unwrap_or(1);
        let matrix = dict
            .get_array("Matrix")
            .ok()
            .and_then(|arr| {
                let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
                (v.len() == 6).then(|| Matrix::new(v[0], v[1], v[2], v[3], v[4], v[5]))
            })
            .unwrap_or_else(Matrix::identity);

        if ptype == 2 {
            // The pattern /Matrix maps pattern space to the defining scope's
            // *default* user space (ISO 32000-1 §8.7.2), so the shading→page
            // transform is scope_base_ctm · matrix.
            let built = dict
                .get("Shading")
                .and_then(|sh_obj| self.build_shading(sh_obj, scope_base_ctm.concat(&matrix)))
                .map(|def| {
                    let avg = def.average_rgb();
                    (std::sync::Arc::new(def), Color::rgb(avg[0], avg[1], avg[2]))
                });
            let bytes = built.as_ref().map_or(0, |(def, _)| def.heap_bytes());
            if self.reserve_content_cache(bytes) {
                self.shading_pattern_cache
                    .insert(shading_key, built.clone());
            }
            return shading_pattern_paint(built.as_ref());
        }

        // Tiling pattern (PatternType 1): capture everything needed to replay
        // the cell at fill time. The gray approximation remains the fallback
        // for paths that cannot take the real paint (e.g. strokes).
        let def = build_tiling_pattern(file, pat_id, &obj, matrix, name);
        let bytes = def.as_ref().map_or(0, |def| def.content.len());
        if self.reserve_content_cache(bytes) {
            self.tiling_pattern_cache.insert(pat_id, def.clone());
        }
        (
            def.map(|def| PatternPaint::Tiling {
                def,
                base_ctm: scope_base_ctm,
            }),
            Color::gray(0.5),
        )
    }

    /// For an uncolored (PaintType 2) tiling pattern selected by `scn`, pop the
    /// color operands that precede the pattern name and convert them in the
    /// Pattern colorspace's base space.
    fn uncolored_pattern_color(
        &mut self,
        pattern: &Option<PatternPaint>,
        base: Option<&ActiveColorSpace>,
    ) -> Option<Color> {
        let Some(PatternPaint::Tiling { def, .. }) = pattern else {
            return None;
        };
        if def.paint_type != 2 {
            return None;
        }
        let base = base?.clone();
        if self.operand_stack.len() < base.components() {
            return None;
        }
        Some(self.pop_color(&base))
    }

    /// Build an evaluatable shading from a /Shading dict (type 2/3 only).
    /// `to_page` maps shading space to page space.
    fn build_shading(
        &mut self,
        obj: &PdfObject,
        to_page: Matrix,
    ) -> Option<crate::shading::ShadingDef> {
        use crate::shading::{ShadingDef, ShadingKind};
        let dict = self.resolve_dict_of(obj)?;
        let shading_type = dict.get_i64("ShadingType").ok()?;
        // Mesh shadings (types 4–7) are streams with a packed vertex/patch
        // bit-stream — a different decode path with no /Coords or required
        // /Function, so branch before the axial/radial logic below.
        if matches!(shading_type, 4..=7) {
            return self.build_mesh_shading(obj, &dict, shading_type, to_page);
        }
        let coords = dict.get("Coords").map(|o| self.resolve_plain(o))?;
        let coords: Vec<f64> = coords
            .as_array()
            .ok()?
            .iter()
            .filter_map(|o| o.as_f64().ok())
            .collect();
        let kind = match shading_type {
            2 if coords.len() >= 4 => ShadingKind::Axial {
                x0: coords[0],
                y0: coords[1],
                x1: coords[2],
                y1: coords[3],
            },
            3 if coords.len() >= 6 => ShadingKind::Radial {
                x0: coords[0],
                y0: coords[1],
                r0: coords[2],
                x1: coords[3],
                y1: coords[4],
                r1: coords[5],
            },
            other => {
                tracing::debug!("unsupported shading type {other}");
                return None;
            }
        };

        let cs = dict
            .get("ColorSpace")
            .map(|o| self.resolve_color_space_obj(o, 0))
            .unwrap_or(ActiveColorSpace::DeviceRGB);

        // /Function: one m-out function or an array of n 1-out functions.
        let func_obj = dict.get("Function")?;
        let funcs: Vec<zpdf_color::PdfFunction> = match self.resolve_plain(func_obj) {
            PdfObject::Array(arr) => arr.iter().filter_map(|o| self.parse_function(o)).collect(),
            _ => self.parse_function(func_obj).into_iter().collect(),
        };
        if funcs.is_empty() {
            return None;
        }

        let domain = dict
            .get_array("Domain")
            .ok()
            .and_then(|a| {
                let v: Vec<f64> = a.iter().filter_map(|o| o.as_f64().ok()).collect();
                (v.len() >= 2).then(|| [v[0], v[1]])
            })
            .unwrap_or([0.0, 1.0]);

        let mut lut = Vec::with_capacity(256);
        for i in 0..256 {
            let t = domain[0] + (i as f64 / 255.0) * (domain[1] - domain[0]);
            let mut comps: Vec<f64> = Vec::new();
            if funcs.len() == 1 {
                comps = funcs[0].eval(&[t]).unwrap_or_default();
            } else {
                for f in &funcs {
                    comps.push(f.eval(&[t]).and_then(|v| v.first().copied()).unwrap_or(0.0));
                }
            }
            let c = self.components_to_rgb(&cs, &comps);
            lut.push([c.r, c.g, c.b]);
        }

        let (extend_start, extend_end) = dict
            .get_array("Extend")
            .ok()
            .map(|a| {
                let b = |i: usize| matches!(a.get(i), Some(PdfObject::Bool(true)));
                (b(0), b(1))
            })
            .unwrap_or((false, false));

        Some(ShadingDef {
            kind,
            lut,
            extend_start,
            extend_end,
            to_page,
        })
    }

    fn build_type3_shading(
        &mut self,
        shading: &zpdf_font::Type3Shading,
        to_page: Matrix,
    ) -> Option<crate::shading::ShadingDef> {
        use crate::shading::{ShadingDef, ShadingKind};
        let dict = &shading.dict;
        let shading_type = dict.get_i64("ShadingType").ok()?;
        let funcs: Vec<zpdf_color::PdfFunction> = shading
            .functions
            .iter()
            .filter_map(Self::parse_type3_function)
            .collect();
        if matches!(shading_type, 4..=7) {
            let data = shading.stream_data.as_deref()?;
            return self.build_mesh_shading_from_decoded(dict, data, &funcs, shading_type, to_page);
        }
        if funcs.is_empty() {
            return None;
        }
        let coords: Vec<f64> = dict
            .get_array("Coords")
            .ok()?
            .iter()
            .filter_map(|o| o.as_f64().ok())
            .collect();
        let kind = match shading_type {
            2 if coords.len() >= 4 => ShadingKind::Axial {
                x0: coords[0],
                y0: coords[1],
                x1: coords[2],
                y1: coords[3],
            },
            3 if coords.len() >= 6 => ShadingKind::Radial {
                x0: coords[0],
                y0: coords[1],
                r0: coords[2],
                x1: coords[3],
                y1: coords[4],
                r1: coords[5],
            },
            other => {
                tracing::debug!("unsupported Type 3 shading type {other}");
                return None;
            }
        };
        let cs = dict
            .get("ColorSpace")
            .map(|o| self.resolve_color_space_obj(o, 0))
            .unwrap_or(ActiveColorSpace::DeviceRGB);
        let domain = dict
            .get_array("Domain")
            .ok()
            .and_then(|a| {
                let v: Vec<f64> = a.iter().filter_map(|o| o.as_f64().ok()).collect();
                (v.len() >= 2).then(|| [v[0], v[1]])
            })
            .unwrap_or([0.0, 1.0]);
        let mut lut = Vec::with_capacity(256);
        for i in 0..256 {
            let t = domain[0] + (i as f64 / 255.0) * (domain[1] - domain[0]);
            let mut comps: Vec<f64> = Vec::new();
            if funcs.len() == 1 {
                comps = funcs[0].eval(&[t]).unwrap_or_default();
            } else {
                for f in &funcs {
                    comps.push(f.eval(&[t]).and_then(|v| v.first().copied()).unwrap_or(0.0));
                }
            }
            let c = self.components_to_rgb(&cs, &comps);
            lut.push([c.r, c.g, c.b]);
        }
        let (extend_start, extend_end) = dict
            .get_array("Extend")
            .ok()
            .map(|a| {
                let b = |i: usize| matches!(a.get(i), Some(PdfObject::Bool(true)));
                (b(0), b(1))
            })
            .unwrap_or((false, false));

        Some(ShadingDef {
            kind,
            lut,
            extend_start,
            extend_end,
            to_page,
        })
    }

    /// Build a mesh shading (types 4–7) from its stream. Decodes the packed
    /// vertex/patch bit-stream, resolves each vertex colour to RGB (applying any
    /// `/Function` then the colour space), tessellates patches, and transforms
    /// the triangles into page space. Both backends consume the rasterized
    /// image, so this needs no backend support.
    fn build_mesh_shading(
        &mut self,
        obj: &PdfObject,
        dict: &zpdf_core::PdfDict,
        shading_type: i64,
        to_page: Matrix,
    ) -> Option<crate::shading::ShadingDef> {
        // Mesh shadings are stream objects; get the decoded bytes.
        let resolved = self.resolve_plain(obj);
        let PdfObject::Stream(stream) = &resolved else {
            tracing::debug!("mesh shading type {shading_type} is not a stream");
            return None;
        };
        let data = match (obj, self.file) {
            (PdfObject::Ref(id), Some(file)) => file
                .resolve_stream_data(*id)
                .or_else(|_| decode_stream_for_file(Some(file), &stream.data, &stream.dict))
                .ok()?,
            _ => decode_stream_for_file(self.file, &stream.data, &stream.dict).ok()?,
        };
        let funcs: Vec<zpdf_color::PdfFunction> = match dict.get("Function") {
            Some(f) => match self.resolve_plain(f) {
                PdfObject::Array(arr) => {
                    arr.iter().filter_map(|o| self.parse_function(o)).collect()
                }
                _ => self.parse_function(f).into_iter().collect(),
            },
            None => Vec::new(),
        };
        self.build_mesh_shading_from_decoded(dict, &data, &funcs, shading_type, to_page)
    }

    fn build_mesh_shading_from_decoded(
        &mut self,
        dict: &zpdf_core::PdfDict,
        data: &[u8],
        funcs: &[zpdf_color::PdfFunction],
        shading_type: i64,
        to_page: Matrix,
    ) -> Option<crate::shading::ShadingDef> {
        use crate::shading::{MeshTriangle, MeshVertex, ShadingDef, ShadingKind};

        let bits_coord = dict.get_i64("BitsPerCoordinate").ok()? as u32;
        let bits_comp = dict.get_i64("BitsPerComponent").ok()? as u32;
        let bits_flag = dict.get_i64("BitsPerFlag").unwrap_or(8) as u32;
        if !(1..=32).contains(&bits_coord) || !(1..=16).contains(&bits_comp) {
            return None;
        }
        let vertices_per_row = dict.get_i64("VerticesPerRow").unwrap_or(0).max(0) as usize;

        let cs = dict
            .get("ColorSpace")
            .map(|o| self.resolve_color_space_obj(o, 0))
            .unwrap_or(ActiveColorSpace::DeviceRGB);
        // One parametric value per vertex with a /Function, else N colour comps.
        let n_color = if funcs.is_empty() { cs.components() } else { 1 };
        if n_color == 0 || n_color > 32 {
            return None;
        }

        let decode: Vec<f64> = dict
            .get_array("Decode")
            .ok()?
            .iter()
            .filter_map(|o| o.as_f64().ok())
            .collect();
        if decode.len() < 4 + 2 * n_color {
            return None;
        }

        let params = crate::mesh::MeshParams {
            bits_flag,
            bits_coord,
            bits_comp,
            n_color,
            decode,
            vertices_per_row,
        };

        // Resolve raw vertex components → device RGB. `this` is an immutable
        // reborrow so the closure can call `components_to_rgb` while the decoder
        // owns the only mutable path.
        let this: &Self = &*self;
        let mut resolve = |comps: &[f64]| -> [f32; 3] {
            let out: Vec<f64> = if funcs.is_empty() {
                comps.to_vec()
            } else if funcs.len() == 1 {
                funcs[0].eval(&[comps[0]]).unwrap_or_default()
            } else {
                funcs
                    .iter()
                    .map(|f| {
                        f.eval(&[comps[0]])
                            .and_then(|v| v.first().copied())
                            .unwrap_or(0.0)
                    })
                    .collect()
            };
            let c = this.components_to_rgb(&cs, &out);
            [c.r, c.g, c.b]
        };

        let tris = crate::mesh::decode_mesh(shading_type, data, &params, &mut resolve);
        if tris.is_empty() {
            return None;
        }

        // Transform shading-space triangles to page space; carry the mean colour
        // in `lut` so `average_rgb()` (pattern-stroke fallback) still works.
        let mut triangles = Vec::with_capacity(tris.len());
        let mut acc = [0.0f64; 3];
        let nverts = (tris.len() * 3).max(1) as f64;
        for t in &tris {
            let mut v = [MeshVertex {
                x: 0.0,
                y: 0.0,
                rgb: [0.0; 3],
            }; 3];
            for (k, &(sx, sy, rgb)) in t.iter().enumerate() {
                let p = Point::new(sx, sy).transform(&to_page);
                v[k] = MeshVertex {
                    x: p.x as f32,
                    y: p.y as f32,
                    rgb,
                };
                acc[0] += rgb[0] as f64;
                acc[1] += rgb[1] as f64;
                acc[2] += rgb[2] as f64;
            }
            triangles.push(MeshTriangle { v });
        }
        let mean = [
            (acc[0] / nverts) as f32,
            (acc[1] / nverts) as f32,
            (acc[2] / nverts) as f32,
        ];

        Some(ShadingDef {
            kind: ShadingKind::Mesh { triangles },
            lut: vec![mean],
            extend_start: false,
            extend_end: false,
            to_page,
        })
    }

    /// Normalize `r` and return `Some` only when all four coordinates are finite.
    fn finite_rect(r: Rect) -> Option<Rect> {
        let r = r.normalize();
        (r.x0.is_finite() && r.y0.is_finite() && r.x1.is_finite() && r.y1.is_finite()).then_some(r)
    }

    /// If `path` is a single simple, non-degenerate, axis-aligned rectangle in
    /// page space, return its normalized bounding [`Rect`].
    ///
    /// Both `FillRule::NonZero` and `FillRule::EvenOdd` fill such a path
    /// identically over the entire rectangle with no holes or uncovered corners.
    /// Rotated/skewed quads, bowties, curves, degenerate/empty rects, non-finite
    /// coordinates, and multi-subpath paths all return `None`.
    fn axis_aligned_rect(path: &Path) -> Option<Rect> {
        let mut iter = path.iter();
        let PathElement::MoveTo(p0) = iter.next()? else {
            return None;
        };
        let PathElement::LineTo(p1) = iter.next()? else {
            return None;
        };
        let PathElement::LineTo(p2) = iter.next()? else {
            return None;
        };
        let PathElement::LineTo(p3) = iter.next()? else {
            return None;
        };
        match iter.next() {
            // 5-element closed rect (`re` or `m l l l h`).
            Some(PathElement::Close) => {
                if iter.next().is_some() {
                    return None;
                }
            }
            // Explicit return to the start vertex (`m l l l l h` or `m l l l l`).
            Some(PathElement::LineTo(p4)) if p4 == p0 => match iter.next() {
                None => {}
                Some(PathElement::Close) if iter.next().is_none() => {}
                _ => return None,
            },
            // 4-element unclosed rect (`m l l l`); PDF clipping implicitly closes subpaths.
            None => {}
            _ => return None,
        }

        if ![p0, p1, p2, p3]
            .iter()
            .all(|p| p.x.is_finite() && p.y.is_finite())
        {
            return None;
        }

        // Consecutive edges must strictly alternate horizontal and vertical
        // with non-zero span on both axes (rejects rotations, skews, bowties,
        // and zero-area rects, while allowing 0/90/180/270-degree transforms).
        let horiz_first = p0.y == p1.y
            && p1.x == p2.x
            && p2.y == p3.y
            && p3.x == p0.x
            && p0.x != p1.x
            && p1.y != p2.y;
        let vert_first = p0.x == p1.x
            && p1.y == p2.y
            && p2.x == p3.x
            && p3.y == p0.y
            && p0.y != p1.y
            && p1.x != p2.x;
        if !horiz_first && !vert_first {
            return None;
        }

        let x0 = p0.x.min(p2.x);
        let x1 = p0.x.max(p2.x);
        let y0 = p0.y.min(p2.y);
        let y1 = p0.y.max(p2.y);
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1, y1))
    }

    /// Destination raster parameters for scale-exact clip elision, matching
    /// `CpuRenderer::fit_raster` and `CpuRenderer::finish_within_limits`.
    ///
    /// Returns `(scale, rect_x0, rect_y1, pw, ph, coord_limit)`, or `None` when
    /// elision is disabled (`clip_elision_scale == 0.0`) or `page_rect` is
    /// degenerate, inverted, non-finite, or too large for exact `f32` pixels.
    fn clip_elision_raster(&self) -> Option<(f32, f32, f32, u32, u32, f32)> {
        let scale = self.clip_elision_scale;
        if !(scale.is_finite() && scale > 0.0) {
            return None;
        }
        let raw = self.display_list.page_rect;
        if !(raw.x0.is_finite()
            && raw.y0.is_finite()
            && raw.x1.is_finite()
            && raw.y1.is_finite()
            && raw.x1 > raw.x0
            && raw.y1 > raw.y0)
        {
            return None;
        }
        let (rect_x0, rect_y1) = (raw.x0 as f32, raw.y1 as f32);
        if !rect_x0.is_finite() || !rect_y1.is_finite() {
            return None;
        }
        let pw_f = ((raw.x1 - raw.x0) * scale as f64).ceil().max(1.0);
        let ph_f = ((raw.y1 - raw.y0) * scale as f64).ceil().max(1.0);
        // Keep u32 -> f32 exact (<= 2^24) so device-pixel comparisons never round.
        const MAX_EXACT_F32_PX: f64 = (1u32 << 24) as f64;
        if !(pw_f.is_finite()
            && ph_f.is_finite()
            && pw_f <= MAX_EXACT_F32_PX
            && ph_f <= MAX_EXACT_F32_PX)
        {
            return None;
        }
        let pw = pw_f as u32;
        let ph = ph_f as u32;
        let span = pw.max(ph) as f32;
        let limit = (span * 64.0).max(1_000_000.0);
        Some((scale, rect_x0, rect_y1, pw, ph, limit))
    }

    /// True when `path` is a single page-space axis-aligned rectangle that
    /// completely covers every affected device pixel of an already-active
    /// enforced clip mask, inspired by `hayro-interpret/src/context.rs`.
    ///
    /// Never elides when no enforced clip mask is active yet
    /// (`clip_elision_bounds == None`): in `tiny-skia`, `RasterPipelineBlitter`
    /// strength-reduces opaque `SourceOver` to `Source` only when
    /// `mask.is_none()`, which changes antialiased edge rounding (single lerp
    /// rounding vs separate source/destination rounding). Keeping the first
    /// mask preserves masked compositing arithmetic while still eliding every
    /// redundant covering rectangle nested beneath it.
    fn clip_path_covers_current_bounds(&self, path: &Path) -> bool {
        let Some((scale, rect_x0, rect_y1, _pw, _ph, limit)) = self.clip_elision_raster() else {
            return false;
        };
        let Some(need) = self.current.clip_elision_bounds else {
            return false;
        };
        if need.x1 <= need.x0 || need.y1 <= need.y0 {
            return false;
        }
        let Some(rect) = Self::axis_aligned_rect(path) else {
            return false;
        };
        let r_left = (rect.x0 as f32 - rect_x0) * scale;
        let r_right = (rect.x1 as f32 - rect_x0) * scale;
        let r_top = (rect_y1 - rect.y1 as f32) * scale;
        let r_bottom = (rect_y1 - rect.y0 as f32) * scale;
        // tiny-skia 0.12's AA scan converter (`scan::path_aa::fill_path` ->
        // `scan::path::fill_path_impl`) shifts unclamped `IntRect` `top`/`bottom`
        // left by `SUPERSAMPLE_SHIFT = 2` in signed `i32` before clamping to
        // the tile, so coordinates outside `[-(1 << 29), (1 << 29) - 1]` wrap
        // and leave the mask empty. `DrawTiler` also translates by up to
        // `-(dim - 1)` per tile (with `f32` round-trip drift above `2^24`).
        // Capping every edge at half the supersampled `i32` range (`±2^28`)
        // leaves >15x headroom over `MAX_EXACT_F32_PX` (`2^24`) for tile
        // offsets and guarantees `IntRect::round_out` and `<< 2` never overflow.
        const MAX_TINY_SKIA_AA_COORD: f32 = (1u32 << 28) as f32;
        let max_coord = limit.min(MAX_TINY_SKIA_AA_COORD);
        if !(r_left.is_finite()
            && r_right.is_finite()
            && r_top.is_finite()
            && r_bottom.is_finite()
            && r_left < r_right
            && r_top < r_bottom
            && r_left.abs() <= max_coord
            && r_right.abs() <= max_coord
            && r_top.abs() <= max_coord
            && r_bottom.abs() <= max_coord)
        {
            return false;
        }
        r_left <= need.x0 as f32
            && r_top <= need.y0 as f32
            && r_right >= need.x1 as f32
            && r_bottom >= need.y1 as f32
    }

    /// Device-pixel footprint (`[x0, x1) × [y0, y1)`) of every raster pixel
    /// `path` can touch, or `None` when clip elision is disabled or the CPU
    /// backend might skip `path` (non-finite, geometry-less, missing leading
    /// `MoveTo`, or exceeding `CpuRenderer::finish_within_limits`).
    fn clip_path_enforced_device_bounds(&self, path: &Path) -> Option<DeviceClipBounds> {
        let (scale, rect_x0, rect_y1, pw, ph, limit) = self.clip_elision_raster()?;
        let mut iter = path.iter();
        if !matches!(iter.next(), Some(PathElement::MoveTo(_))) {
            return None;
        }
        let has_edge = iter.any(|e| matches!(e, PathElement::LineTo(_) | PathElement::CurveTo(..)));
        if !has_edge {
            return None;
        }
        let (mut left, mut top) = (f32::INFINITY, f32::INFINITY);
        let (mut right, mut bottom) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for p in path.points() {
            if !p.x.is_finite() || !p.y.is_finite() {
                return None;
            }
            let px = (p.x as f32 - rect_x0) * scale;
            let py = (rect_y1 - p.y as f32) * scale;
            if !px.is_finite() || !py.is_finite() || px.abs() > limit || py.abs() > limit {
                return None;
            }
            left = left.min(px);
            right = right.max(px);
            top = top.min(py);
            bottom = bottom.max(py);
        }
        if !(left.is_finite() && right.is_finite() && top.is_finite() && bottom.is_finite()) {
            return None;
        }
        let x0 = (left.floor().max(0.0) as u32).min(pw);
        let x1 = (right.ceil().max(0.0) as u32).min(pw);
        let y0 = (top.floor().max(0.0) as u32).min(ph);
        let y1 = (bottom.ceil().max(0.0) as u32).min(ph);
        Some(DeviceClipBounds { x0, y0, x1, y1 })
    }

    /// Intersect the running device-pixel clip elision bounds with an enforced clip.
    fn intersect_clip_elision_bounds(&mut self, new: Option<DeviceClipBounds>) {
        let Some(n) = new else {
            return;
        };
        self.current.clip_elision_bounds = Some(match self.current.clip_elision_bounds {
            Some(c) => {
                let x0 = c.x0.max(n.x0);
                let y0 = c.y0.max(n.y0);
                DeviceClipBounds {
                    x0,
                    y0,
                    x1: c.x1.min(n.x1).max(x0),
                    y1: c.y1.min(n.y1).max(y0),
                }
            }
            None => n,
        });
    }

    /// Intersect the running clip-bounds with a new clip path's page-space bbox.
    fn intersect_clip_bounds(&mut self, new: Option<Rect>) {
        let Some(n) = new.and_then(Self::finite_rect) else {
            return;
        };
        self.current.clip_bounds =
            Some(match self.current.clip_bounds.and_then(Self::finite_rect) {
                Some(c) => {
                    let (x0, y0) = (c.x0.max(n.x0), c.y0.max(n.y0));
                    // Disjoint clips leave an empty region. Keep it empty (zero
                    // area at its corner): an inverted rect would `normalize()`
                    // back into a non-empty one at the next intersection.
                    Rect::new(x0, y0, c.x1.min(n.x1).max(x0), c.y1.min(n.y1).max(y0))
                }
                None => n,
            });
    }

    /// Page-space region a full-page paint (`sh`) actually needs to cover: the
    /// active clip bounds intersected with the page rect, or the whole page when
    /// unclipped. May be empty (caller skips).
    fn shading_region(&self) -> Rect {
        let page = self.display_list.page_rect.normalize();
        match self.current.clip_bounds {
            Some(c) if self.file.is_none() => {
                // Standalone Type 3 replay uses a placeholder glyph rect rather
                // than a real page MediaBox; honor the effective glyph clip
                // directly so negative overhangs (x < 0) and coordinates beyond
                // the placeholder are preserved.
                if c.x1 <= c.x0 || c.y1 <= c.y0 || !c.width().is_finite() || !c.height().is_finite()
                {
                    Rect::new(0.0, 0.0, 0.0, 0.0)
                } else {
                    c
                }
            }
            Some(c) => {
                let c = c.normalize();
                Rect::new(
                    c.x0.max(page.x0),
                    c.y0.max(page.y0),
                    c.x1.min(page.x1),
                    c.y1.min(page.y1),
                )
            }
            None => page,
        }
    }

    /// `sh` operator: paint the shading across the current clip region (not the
    /// whole page — that rasterizes a full-page gradient per call, which a map
    /// with hundreds of small `sh` markers makes pathologically slow).
    fn paint_shading_op(&mut self, name: &str) {
        if self.oc_suppressed() {
            self.type3_oc_suppressed_mark = true;
            return;
        }
        // Shadings are represented by decoded images in the display list. Text
        // and table extraction intentionally omit an image cache, so avoid all
        // function parsing, mesh construction, and raster work when there is no
        // store in which the resulting pixels could be retained.
        if self.skip_images || (self.image_cache.is_none() && self.active_type3_glyphs.is_empty()) {
            return;
        }
        if self.file.is_none() {
            let Some(t3_shading) = self.lookup_type3_shading(name) else {
                self.stats.diagnostics.missing_resources =
                    self.stats.diagnostics.missing_resources.saturating_add(1);
                tracing::debug!("Type 3 shading {name} not found in resources");
                return;
            };
            let region = self.shading_region();
            if region.width() <= 0.0 || region.height() <= 0.0 {
                return;
            }
            let Some(def) = self.build_type3_shading(&t3_shading, self.current.ctm) else {
                self.stats.diagnostics.unsupported_features = self
                    .stats
                    .diagnostics
                    .unsupported_features
                    .saturating_add(1);
                return;
            };
            self.emit_shading_image(&def, region);
            return;
        }
        let (sh_obj, cache_id) = match self.lookup_res(|r| {
            r.shadings_inline
                .get(name)
                .cloned()
                .map(|o| (o, None))
                .or_else(|| {
                    r.shadings
                        .get(name)
                        .copied()
                        .map(|id| (PdfObject::Ref(id), Some(id)))
                })
        }) {
            Some(found) => found,
            None => {
                self.stats.diagnostics.missing_resources =
                    self.stats.diagnostics.missing_resources.saturating_add(1);
                tracing::debug!("shading {name} not found in resources");
                return;
            }
        };

        // Only rasterize what the clip actually exposes.
        let region = self.shading_region();
        if region.width() <= 0.0 || region.height() <= 0.0 {
            return;
        }

        // For `sh`, shading coordinates live in the current user space.
        let def = match cache_id.and_then(|id| self.shading_cache.get(&id).cloned()) {
            Some(mut d) => {
                d.to_page = self.current.ctm;
                d
            }
            None => {
                let Some(d) = self.build_shading(&sh_obj, self.current.ctm) else {
                    self.stats.diagnostics.unsupported_features = self
                        .stats
                        .diagnostics
                        .unsupported_features
                        .saturating_add(1);
                    return;
                };
                // Mesh defs bake their vertices against the current CTM, so they
                // must not be reused via the to_page-rewrite cache path below.
                if let Some(id) = cache_id {
                    if !matches!(d.kind, crate::shading::ShadingKind::Mesh { .. }) {
                        self.shading_cache.insert(id, d.clone());
                    }
                }
                d
            }
        };
        self.emit_shading_image(&def, region);
    }

    /// Rasterize `def` over `region` (in the current display list's coordinate
    /// space) and emit it as an image, sizing the raster from the transformed
    /// page-space footprint so magnified tiling-pattern cells keep full gradient
    /// detail while placing the image in cell coordinates. Returns `true` when
    /// an image command was emitted.
    fn emit_shading_image(&mut self, def: &crate::shading::ShadingDef, region: Rect) -> bool {
        if self.skip_images || region.width() <= 0.0 || region.height() <= 0.0 {
            return false;
        }
        let embed_inline = self.image_cache.is_none() && !self.active_type3_glyphs.is_empty();
        let m = self.shading_space_to_page;
        let page_w = (m.a * region.width()).hypot(m.b * region.width());
        let page_h = (m.c * region.height()).hypot(m.d * region.height());
        let (eff_w, eff_h) =
            if page_w.is_finite() && page_h.is_finite() && page_w > 0.0 && page_h > 0.0 {
                (page_w, page_h)
            } else {
                (region.width(), region.height())
            };
        // Resolution: smooth gradients upscale cleanly; cap the long side.
        // In standalone Type 3 glyph replay (`self.file.is_none()`), glyph
        // coordinates can be small (e.g. unit-square `[0..1]`) before `font_size`
        // scaling, so rasterize at least 256 px along the long edge to preserve
        // smooth gradients when enlarged on the page.
        let long = eff_w.max(eff_h);
        let scale = if self.file.is_none() {
            (256.0 / long).max(2.0).min(768.0 / long)
        } else {
            (768.0 / long).min(2.0)
        };
        let w = ((eff_w * scale).ceil() as u32).clamp(1, 2048);
        let h = ((eff_h * scale).ceil() as u32).clamp(1, 2048);
        // Gradient/mesh evaluation is CPU work here, and the result reaches a
        // render backend as an ordinary image — so this is the only place a
        // shading's cost can be attributed. (Both backends paint only
        // `Paint::Solid`; a `Paint::Shading` never reaches them.)
        let shading_started = zpdf_core::time::Instant::now();
        let buf = crate::shading::rasterize(def, region, w, h);
        self.stats.shading_ns += shading_started.elapsed().as_nanos() as u64;
        let Some(buf) = buf else {
            return false;
        };
        self.stats.shading_rasters += 1;
        let image = zpdf_image::DecodedImage {
            width: w,
            height: h,
            data: buf,
            has_alpha: true,
            is_image_mask: false,
            premultiplied: true,
        };
        // Unit-square image -> page-space region.
        let transform = Matrix::new(
            region.width(),
            0.0,
            0.0,
            region.height(),
            region.x0,
            region.y0,
        );
        if embed_inline {
            return self.emit_draw_inline_image_at(image, transform).is_some();
        }
        let image = self.fit_image_to_footprint(image, &transform);
        let Some(image_id) = self.admit_image(image) else {
            return false;
        };
        self.emit_image_draw(image_id, transform, false);
        true
    }

    /// Page-space bounding box of a path (for clipping shading fills).
    fn path_bounds(path: &Path) -> Option<Rect> {
        let mut min = Point::new(f64::INFINITY, f64::INFINITY);
        let mut max = Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
        // Every point, including curve control points. Reject any path carrying
        // a non-finite coordinate so clip_bounds never narrows on a path the
        // backend will skip.
        for p in path.points() {
            if !p.x.is_finite() || !p.y.is_finite() {
                return None;
            }
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        (min.x.is_finite()
            && min.y.is_finite()
            && max.x.is_finite()
            && max.y.is_finite()
            && max.x > min.x
            && max.y > min.y)
            .then(|| Rect::new(min.x, min.y, max.x, max.y))
    }

    /// True while painting is suppressed by a hidden optional-content block.
    fn oc_suppressed(&self) -> bool {
        self.oc_hidden_from.is_some()
    }

    /// Visibility of a BDC /OC properties operand: an inline dict, or a name
    /// looked up in the /Properties resource.
    fn oc_properties_visible(&self, props: &PdfObject) -> bool {
        match props {
            PdfObject::Name(n) => match self.lookup_property_resource(n.as_str()) {
                Some(o) => self.oc_object_visible(&o, 0),
                None => true,
            },
            other => self.oc_object_visible(other, 0),
        }
    }

    /// The marked-content id in effect at the current point: the innermost
    /// enclosing marked-content sequence that declares an `/MCID`. A nested
    /// sequence with no `/MCID` inherits the enclosing one (ISO 32000-1 §14.6,
    /// §14.7.4.2). `None` when no enclosing sequence carries one. Used only to
    /// tag extracted text spans so a Tagged-PDF structure element can bind them.
    fn current_mcid(&self) -> Option<i32> {
        self.mc_stack.iter().rev().find_map(|frame| frame.mcid)
    }

    /// A text-only limit leaves the completed spans with their caller. The
    /// interpreter still paints this show and executes the rest of the page.
    fn stop_text_extraction(&mut self) {
        self.stats.diagnostics.resource_limit_reached = true;
        self.text_sink = None;
        self.span_streams = None;
        self.active_actual_text = None;
    }

    fn current_struct_actual_text_covers(&self) -> bool {
        self.enclosing_struct_actual_text
            || (!self.struct_actual_text_mcids.is_empty()
                && self.current_mc_stream.is_some_and(|stream| {
                    self.current_mcid().is_some_and(|mcid| {
                        self.struct_actual_text_mcids
                            .contains(&(stream, mcid as i64))
                    })
                }))
    }

    fn current_span_stream_binding(&self) -> crate::text::SpanStreamBinding {
        if self.enclosing_struct_actual_text {
            crate::text::SpanStreamBinding::Subsumed
        } else if let Some(stream) = self.current_mc_stream {
            crate::text::SpanStreamBinding::Stream(stream)
        } else {
            crate::text::SpanStreamBinding::Unbound
        }
    }

    fn begin_actual_text(&mut self, props: &PdfObject, mcid: Option<i32>) -> Option<ActualText> {
        if self.text_sink.is_none() || self.active_actual_text.is_some() || self.oc_suppressed() {
            return None;
        }
        let dict = self.resolve_property_dict(props)?;
        let value = self.resolve_plain(dict.get("ActualText")?);
        let PdfObject::String(value) = value else {
            return None;
        };
        // Decoding can expand PDFDocEncoding to three UTF-8 bytes per byte.
        // Charge all replacements, including repeatedly referenced properties.
        let remaining = self
            .max_actual_text_bytes
            .saturating_sub(self.actual_text_bytes);
        if value.as_bytes().len() > remaining / 3 {
            self.stop_text_extraction();
            return None;
        }
        let text = zpdf_core::decode_text_string(value.as_bytes());
        if text.len() as u64
            > self.parse_limits.max_decoded_stream_bytes - self.extracted_text_bytes
        {
            self.stop_text_extraction();
            return None;
        }
        self.extracted_text_bytes += text.len() as u64;
        self.actual_text_bytes += text.len();
        let tm = self.current.ctm.concat(&self.text_matrix);
        Some(ActualText {
            span: TextSpan {
                text,
                x: tm.e,
                y: tm.f,
                size: self.current.font_size * self.ctm_scale_factor(),
                advance: 0.0,
                mcid: mcid.or_else(|| self.current_mcid()),
                glyphs: Vec::new(),
            },
            stream_binding: self.current_span_stream_binding(),
            has_source: false,
            following_glyphs: Vec::new(),
        })
    }

    /// Close the sequences a stream left open. Every limit that ends
    /// interpretation is treated alike: a replacement it interrupted has not
    /// seen all of its source glyphs.
    fn close_marked_content(&mut self) {
        let complete = !self.cut_short();
        while !self.mc_stack.is_empty() {
            self.end_marked_content(complete);
        }
    }

    /// Publish the outer replacement. `complete` is false when a limit ended
    /// the stream before the sequence's own `EMC`.
    fn end_marked_content(&mut self, complete: bool) {
        let ends_actual_text = self
            .mc_stack
            .pop()
            .is_some_and(|frame| frame.ends_actual_text);
        if let Some(sink) = self.text_sink.as_mut().filter(|_| ends_actual_text) {
            if let Some(actual) = self.active_actual_text.take() {
                actual.finish(
                    complete,
                    sink,
                    self.span_streams.as_deref_mut(),
                    &mut self.text_glyphs_collected,
                );
            }
        }
        if self.oc_hidden_from == Some(self.mc_depth) {
            self.oc_hidden_from = None;
        }
        self.mc_depth = self.mc_depth.saturating_sub(1);
    }

    /// Read the `/MCID` of a `BDC` properties operand (an inline dict, or a name
    /// resolved through the `/Properties` resource), validated to a non-negative
    /// `i32`. `None` when the sequence declares no usable marked-content id.
    fn marked_content_id(&self, props: &PdfObject) -> Option<i32> {
        let dict = self.resolve_property_dict(props)?;
        match dict.get("MCID") {
            Some(PdfObject::Integer(n)) => i32::try_from(*n).ok().filter(|&m| m >= 0),
            _ => None,
        }
    }

    /// Resolve a `BDC` properties operand to its dictionary: an inline dict, a
    /// name looked up in the `/Properties` resource (inline or by reference), or
    /// a direct reference. Mirrors [`Self::oc_properties_visible`]'s resolution.
    fn resolve_property_dict(&self, props: &PdfObject) -> Option<PdfDict> {
        match props {
            PdfObject::Dict(d) => Some(d.clone()),
            PdfObject::Name(n) => match self.lookup_property_resource(n.as_str())? {
                PdfObject::Dict(d) => Some(d),
                PdfObject::Ref(id) => self.file?.resolve(id).ok()?.as_dict().ok().cloned(),
                _ => None,
            },
            PdfObject::Ref(r) => self.file?.resolve(*r).ok()?.as_dict().ok().cloned(),
            _ => None,
        }
    }

    /// Visibility of an /OC value: an OCG ref, or an OCMD with /OCGs + /P
    /// policy or a /VE visibility expression (which takes precedence).
    fn oc_object_visible(&self, obj: &PdfObject, depth: u8) -> bool {
        if depth > 8 {
            return true;
        }
        let Some(config) = self.oc_config else {
            return true;
        };
        // Track the id so direct OCG refs use set membership.
        let (id, resolved) = match obj {
            PdfObject::Ref(r) => match self.file.map(|f| f.resolve(*r)) {
                Some(Ok(o)) => (Some(*r), o),
                _ => return true,
            },
            other => (None, other.clone()),
        };
        let Ok(dict) = resolved.as_dict() else {
            return true;
        };

        match dict.get_name("Type") {
            Ok("OCMD") => {
                // /VE visibility expression wins over /OCGs per 8.11.2.3.
                if let Some(ve) = dict.get("VE") {
                    return self.oc_expression_visible(ve, depth + 1);
                }
                let groups: Vec<bool> = match dict.get("OCGs") {
                    Some(PdfObject::Array(arr)) => arr
                        .iter()
                        .filter_map(|o| match o {
                            PdfObject::Ref(r) => Some(config.group_visible(*r)),
                            _ => None,
                        })
                        .collect(),
                    Some(PdfObject::Ref(r)) => vec![config.group_visible(*r)],
                    _ => return true,
                };
                if groups.is_empty() {
                    return true;
                }
                match dict.get_name("P") {
                    Ok("AllOn") => groups.iter().all(|&v| v),
                    Ok("AnyOff") => groups.iter().any(|&v| !v),
                    Ok("AllOff") => groups.iter().all(|&v| !v),
                    // Default policy: AnyOn.
                    _ => groups.iter().any(|&v| v),
                }
            }
            // An OCG (or untyped dict): membership is by object id.
            _ => id.map(|i| config.group_visible(i)).unwrap_or(true),
        }
    }

    /// Evaluate a /VE visibility expression: ["Not", x], ["And", ...],
    /// ["Or", ...] with OCG refs (or nested expressions) as operands.
    fn oc_expression_visible(&self, ve: &PdfObject, depth: u8) -> bool {
        if depth > 8 {
            return true;
        }
        let arr = match &self.resolve_plain(ve) {
            PdfObject::Array(a) => a.clone(),
            _ => return true,
        };
        let Some(PdfObject::Name(op)) = arr.first() else {
            return true;
        };
        let operand = |o: &PdfObject| -> bool {
            match o {
                PdfObject::Array(_) => self.oc_expression_visible(o, depth + 1),
                other => self.oc_object_visible(other, depth + 1),
            }
        };
        match op.as_str() {
            "Not" => arr.get(1).map(|o| !operand(o)).unwrap_or(true),
            "And" => arr[1..].iter().all(operand),
            "Or" => arr[1..].iter().any(operand),
            _ => true,
        }
    }

    /// Paint annotation appearance streams (PDF 12.5.5): each /AP form's
    /// /Matrix-transformed /BBox is mapped onto the annotation /Rect.
    fn paint_annotations(&mut self) {
        let Some(annots) = self.annotations.take() else {
            return;
        };
        let Some(file) = self.file else {
            return;
        };
        for a in annots {
            if !a.is_viewable() {
                continue;
            }
            if let Some(oc) = &a.oc {
                if !self.oc_object_visible(oc, 0) {
                    continue;
                }
            }
            // A generated form-field appearance takes precedence over a stored
            // /AP stream. A synthetic stream has no object id (`None`), so the
            // form emitter decodes its bytes directly instead of an xref lookup.
            let (stream, paint_id) = if let Some(gen) = &a.generated {
                (synthetic_form_stream(gen), None)
            } else {
                let Some(ap_id) = a.appearance else { continue };
                let Ok(PdfObject::Stream(s)) = file.resolve(ap_id) else {
                    self.stats.diagnostics.missing_resources =
                        self.stats.diagnostics.missing_resources.saturating_add(1);
                    continue;
                };
                (s, Some(ap_id))
            };

            // Form /BBox, transformed by the form /Matrix, mapped onto /Rect.
            let bbox = stream.dict.get_array("BBox").ok().and_then(|arr| {
                let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
                (v.len() == 4).then(|| {
                    Rect::new(
                        v[0].min(v[2]),
                        v[1].min(v[3]),
                        v[0].max(v[2]),
                        v[1].max(v[3]),
                    )
                })
            });
            let Some(bbox) = bbox.filter(|b| b.width() > 0.0 && b.height() > 0.0) else {
                self.stats.diagnostics.malformed_content =
                    self.stats.diagnostics.malformed_content.saturating_add(1);
                continue;
            };
            let matrix = stream
                .dict
                .get_array("Matrix")
                .ok()
                .and_then(|arr| {
                    let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
                    (v.len() == 6).then(|| Matrix::new(v[0], v[1], v[2], v[3], v[4], v[5]))
                })
                .unwrap_or_else(Matrix::identity);

            // Bounding box of the transformed BBox corners.
            let corners = [
                Point::new(bbox.x0, bbox.y0),
                Point::new(bbox.x1, bbox.y0),
                Point::new(bbox.x0, bbox.y1),
                Point::new(bbox.x1, bbox.y1),
            ];
            let (mut tx0, mut ty0) = (f64::INFINITY, f64::INFINITY);
            let (mut tx1, mut ty1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
            for c in &corners {
                let p = c.transform(&matrix);
                tx0 = tx0.min(p.x);
                ty0 = ty0.min(p.y);
                tx1 = tx1.max(p.x);
                ty1 = ty1.max(p.y);
            }
            let (tw, th) = (tx1 - tx0, ty1 - ty0);
            if !(tw.is_finite() && th.is_finite()) || tw <= 1e-9 || th <= 1e-9 {
                continue;
            }

            // Map the form BBox onto the annotation rect. Normalize first so a
            // legal but inverted /Rect (upper-right corner given first) places
            // the origin at the lower-left corner, not the wrong one.
            let rect = a.rect.normalize();
            let a_mat = Matrix::translate(rect.x0, rect.y0)
                .concat(&Matrix::scale(rect.width() / tw, rect.height() / th))
                .concat(&Matrix::translate(-tx0, -ty0));

            let saved_ctm = self.current.ctm;
            self.current.ctm = self.base_ctm.concat(&a_mat);
            self.do_form_xobject(paint_id, &stream, file);
            self.current.ctm = saved_ctm;
        }
    }

    /// Emit a painting command, bracketed in a blend group when a non-Normal
    /// blend mode or a soft mask is active (backends composite the group with
    /// that mode/mask).
    fn emit_painted(&mut self, cmd: RenderCommand) {
        if self.oc_suppressed() {
            self.type3_oc_suppressed_mark = true;
            return;
        }
        if self.current.blend_mode != BlendMode::Normal || self.current.soft_mask.is_some() {
            // A backdrop-dependent tiling pattern painted under an outer blend
            // mode and/or soft mask must evaluate its cell against the live page
            // backdrop BEFORE the outer blend mode and soft mask are applied
            // (ISO 32000-1 §11.6.7), so open a non-isolated group instead of
            // isolating from the page backdrop.
            let non_isolated_tiling = match &cmd {
                RenderCommand::FillPath {
                    paint: Paint::Tiling(t),
                    ..
                }
                | RenderCommand::StrokePath {
                    paint: Paint::Tiling(t),
                    ..
                } => t.is_backdrop_dependent(),
                RenderCommand::DrawGlyphRun(run) => match &run.paint {
                    Paint::Tiling(t) => t.is_backdrop_dependent(),
                    _ => false,
                },
                _ => false,
            };
            self.display_list.push(RenderCommand::PushBlendGroup {
                blend_mode: self.current.blend_mode,
                // A single solid/backdrop-independent object carrying a blend
                // mode or soft mask is equivalent to an isolated group of one
                // element composited with that mode — it must NOT pull in the
                // backdrop (which would change a semi-transparent blended fill).
                isolated: !non_isolated_tiling,
                knockout: false,
                bounds: self.display_list.page_rect,
                alpha: 1.0,
                mask: self.current.soft_mask.clone(),
            });
            self.display_list.push(cmd);
            self.display_list.push(RenderCommand::PopBlendGroup);
        } else {
            self.display_list.push(cmd);
        }
    }

    fn apply_ext_gstate(&mut self, name: &str) {
        enum LookedUpGs {
            Inline(PdfDict),
            Ref(ObjectId),
        }
        if self.file.is_none() {
            if let Some((dict, smask)) = self.lookup_type3_ext_gstate(name) {
                self.apply_ext_gstate_dict(&dict, None);
                if let Some(smask) = smask {
                    self.current.soft_mask = match smask {
                        zpdf_font::Type3SoftMask::None => None,
                        zpdf_font::Type3SoftMask::Dict {
                            is_alpha,
                            group,
                            backdrop_luma,
                            transfer,
                        } => self.build_type3_soft_mask(
                            is_alpha,
                            &group,
                            backdrop_luma,
                            transfer.as_ref(),
                        ),
                    };
                }
                return;
            }
        }

        // Check both inline and indirect entries within each scope before
        // advancing to its parent so an outer inline /GS never shadows an
        // inner indirect /GS.
        match self.lookup_res(|r| {
            r.ext_g_state_inline
                .get(name)
                .cloned()
                .map(LookedUpGs::Inline)
                .or_else(|| r.ext_g_state.get(name).copied().map(LookedUpGs::Ref))
        }) {
            Some(LookedUpGs::Inline(dict)) => {
                self.apply_ext_gstate_dict(&dict, None);
            }
            Some(LookedUpGs::Ref(gs_id)) => {
                let Some(file) = self.file else {
                    return;
                };
                let obj = match file.resolve_shared(gs_id) {
                    Ok(o) => o,
                    Err(e) => {
                        self.stats.diagnostics.record_decode_failure(&e);
                        return;
                    }
                };
                if let Ok(dict) = obj.as_dict() {
                    self.apply_ext_gstate_dict(dict, Some(gs_id));
                }
            }
            None => {
                if self.file.is_some() {
                    self.stats.diagnostics.missing_resources =
                        self.stats.diagnostics.missing_resources.saturating_add(1);
                }
            }
        }
    }

    /// `gs_id` identifies the indirect ExtGState (None for inline dicts);
    /// it keys tiling-pattern soft-mask reuse.
    fn apply_ext_gstate_dict(&mut self, dict: &zpdf_core::PdfDict, gs_id: Option<ObjectId>) {
        if let Ok(a) = dict.get_f64("ca") {
            self.current.fill_alpha = a as f32;
        }
        if let Ok(a) = dict.get_f64("CA") {
            self.current.stroke_alpha = a as f32;
        }
        // Overprint (8.6.7): /OP = stroking, /op = nonstroking. Per spec, if
        // /op is absent, /OP also sets the nonstroking parameter. Ignored when
        // colour-related state is locked (uncoloured pattern or Type 3 `d1`).
        if !self.suppress_color_ops {
            let op_for_stroke = match dict.get("OP") {
                Some(PdfObject::Bool(b)) => Some(*b),
                _ => None,
            };
            if let Some(b) = op_for_stroke {
                self.current.stroke_overprint = b;
            }
            match dict.get("op") {
                Some(PdfObject::Bool(b)) => self.current.fill_overprint = *b,
                _ => {
                    if let Some(b) = op_for_stroke {
                        self.current.fill_overprint = b;
                    }
                }
            }
            // /OPM: overprint mode (only 0 and 1 are defined; clamp others to 1).
            // Read as a number so a real-valued `/OPM 1.0` (some producers emit it)
            // is honoured, not just an integer.
            if let Ok(m) = dict.get_f64("OPM") {
                self.current.overprint_mode = if m.round() == 0.0 { 0 } else { 1 };
            }
        }
        if let Ok(w) = dict.get_f64("LW") {
            self.current.line_width = w as f32;
        }
        if let Ok(c) = dict.get_i64("LC") {
            self.current.line_cap = match c as u8 {
                1 => LineCap::Round,
                2 => LineCap::Square,
                _ => LineCap::Butt,
            };
        }
        if let Ok(j) = dict.get_i64("LJ") {
            self.current.line_join = match j as u8 {
                1 => LineJoin::Round,
                2 => LineJoin::Bevel,
                _ => LineJoin::Miter,
            };
        }
        if let Ok(m) = dict.get_f64("ML") {
            self.current.miter_limit = m as f32;
        }
        // /D: `[dash-array phase]`, as the `d` operator takes them.
        if let Some(PdfObject::Array(d)) = dict.get("D").map(|o| self.resolve_plain(o)) {
            if let Some(PdfObject::Array(arr)) = d.first().map(|o| self.resolve_plain(o)) {
                let array: Vec<f32> = arr
                    .iter()
                    .filter_map(|o| o.as_f64().ok().map(|v| v as f32))
                    .collect();
                let phase = d.get(1).and_then(|o| o.as_f64().ok()).unwrap_or(0.0) as f32;
                self.current.dash = (!array.is_empty()).then_some(DashPattern { array, phase });
            }
        }
        // /Font: `[font-dict-ref size]`, selecting the font as `Tf` would.
        if let Some(PdfObject::Array(f)) = dict.get("Font").map(|o| self.resolve_plain(o)) {
            if let (Some(PdfObject::Ref(font_ref)), Some(size)) =
                (f.first(), f.get(1).and_then(|o| o.as_f64().ok()))
            {
                self.select_font_object(*font_ref, size as f32);
            }
        }
        // /RI: rendering intent name (ignored when colour-related state is locked).
        if !self.suppress_color_ops {
            if let Ok(ri) = dict.get_name("RI") {
                self.current.rendering_intent = zpdf_color::RenderIntent::from_pdf_name(ri);
            }
        }
        // /BM: a name or an array of names (use the first supported one).
        let bm_name = match dict.get("BM") {
            Some(PdfObject::Name(n)) => Some(n.0.clone()),
            Some(PdfObject::Array(arr)) => arr.iter().find_map(|o| match o {
                PdfObject::Name(n) => Some(n.0.clone()),
                _ => None,
            }),
            _ => None,
        };
        if let Some(bm) = bm_name {
            self.current.blend_mode = match bm.as_str() {
                "Multiply" => BlendMode::Multiply,
                "Screen" => BlendMode::Screen,
                "Overlay" => BlendMode::Overlay,
                "Darken" => BlendMode::Darken,
                "Lighten" => BlendMode::Lighten,
                "ColorDodge" => BlendMode::ColorDodge,
                "ColorBurn" => BlendMode::ColorBurn,
                "HardLight" => BlendMode::HardLight,
                "SoftLight" => BlendMode::SoftLight,
                "Difference" => BlendMode::Difference,
                "Exclusion" => BlendMode::Exclusion,
                "Hue" => BlendMode::Hue,
                "Saturation" => BlendMode::Saturation,
                "Color" => BlendMode::Color,
                "Luminosity" => BlendMode::Luminosity,
                _ => BlendMode::Normal,
            };
        }
        // /SMask: a soft-mask dict, or the name /None to clear.
        match dict.get("SMask").map(|o| self.resolve_plain(o)) {
            Some(PdfObject::Name(n)) if n.as_str() == "None" => {
                self.current.soft_mask = None;
            }
            Some(PdfObject::Dict(sm)) => {
                self.current.soft_mask = self.build_or_reuse_soft_mask(&sm, gs_id);
            }
            _ => {}
        }
    }

    /// Resolve an ExtGState `/SMask`, reusing a cached [`SoftMask`] when the
    /// same ExtGState object is applied under identical inherited graphics
    /// state and resource scopes.
    fn build_or_reuse_soft_mask(
        &mut self,
        sm: &zpdf_core::PdfDict,
        gs_id: Option<ObjectId>,
    ) -> Option<SoftMask> {
        let Some(gs_id) = gs_id else {
            return self.build_soft_mask(sm);
        };
        let Some(inherited) =
            InheritedGraphicsStateKey::from_state(&self.current, self.current_font_id)
        else {
            return self.build_soft_mask(sm);
        };
        let (ctm, base) = (&self.current.ctm, &self.base_ctm);
        let key = SoftMaskCacheKey {
            gs_id,
            matrices: [
                ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f, base.a, base.b, base.c, base.d, base.e,
                base.f,
            ]
            .map(f64::to_bits),
            clip: self
                .current
                .clip_bounds
                .map(|r| [r.x0, r.y0, r.x1, r.y1].map(f64::to_bits)),
            inherited,
            resource_scopes: self.form_resource_scopes.clone(),
            embed_type3_inline_images: self.image_cache.is_none()
                && !self.active_type3_glyphs.is_empty(),
        };
        if let Some(mask) = self.soft_mask_cache.get(&key) {
            return mask.clone();
        }
        let mask = self.build_soft_mask(sm);
        if self.soft_mask_cache.len() < MAX_CACHED_SOFT_MASKS {
            self.soft_mask_cache.insert(key, mask.clone());
        }
        mask
    }

    fn resource_scope_id(
        &mut self,
        res_obj: &PdfObject,
        owner_id: Option<ObjectId>,
    ) -> ResourceScopeId {
        if let PdfObject::Ref(id) = res_obj {
            ResourceScopeId::ResourcesObject(*id)
        } else if let Some(id) = owner_id {
            ResourceScopeId::OwnerObject(id)
        } else {
            let id = self.next_resource_scope_id;
            self.next_resource_scope_id = self.next_resource_scope_id.wrapping_add(1);
            ResourceScopeId::Unique(id)
        }
    }

    /// Build a [`SoftMask`] from an ExtGState /SMask dict: interpret the /G
    /// transparency group into its own command list (geometry fixed at `gs`
    /// time), pre-sample /TR, and approximate the /BC backdrop luminosity.
    fn build_soft_mask(&mut self, sm: &zpdf_core::PdfDict) -> Option<SoftMask> {
        let file = self.file?;
        let kind = match sm.get_name("S") {
            Ok("Alpha") => SoftMaskKind::Alpha,
            _ => SoftMaskKind::Luminosity,
        };
        let g_ref = sm.get_ref("G").ok()?;
        let g_obj = file.resolve(g_ref).ok()?;
        let PdfObject::Stream(g_stream) = g_obj else {
            tracing::debug!("/SMask /G is not a stream");
            return None;
        };

        // Interpret the mask group into a detached command list with a clean
        // paint state — the mask itself is not blended, masked, or alpha'd,
        // and a surrounding hidden BDC /OC block must not blank it (the gs
        // takes effect later, possibly in visible content).
        let page_rect = self.display_list.page_rect;
        let saved_dl = std::mem::replace(&mut self.display_list, DisplayList::new(page_rect));
        let saved_state = self.current.clone();
        let saved_oc_hidden = self.oc_hidden_from.take();
        // Mask glyphs and rules define opacity, not logical page content.
        // Keep /ActualText in those painting resources out of extraction too.
        let saved_text_sink = self.text_sink.take();
        let saved_span_streams = self.span_streams.take();
        let saved_rule_sink = self.rule_sink.take();
        let saved_type3_glyph = std::mem::replace(&mut self.type3_glyph, false);
        let saved_suppress_color_ops = std::mem::replace(&mut self.suppress_color_ops, false);
        self.current.soft_mask = None;
        self.current.blend_mode = BlendMode::Normal;
        self.current.fill_alpha = 1.0;
        self.current.stroke_alpha = 1.0;
        // The detached mask display list starts unclipped; clear inherited
        // clip bounds and clip_elision_bounds so clips and shadings inside /G
        // are never clipped or elided against the outer page's clip (which is
        // not pushed onto mask_dl).
        self.current.clip_bounds = None;
        self.current.clip_elision_bounds = None;
        // A soft mask's /G group *is* a form, drawn into a detached command
        // list — attributed as form work, which is where a mask-heavy page's
        // interpret time will show up.
        let work = self.start_work(Some(Work::Form));
        self.do_form_xobject(Some(g_ref), &g_stream, file);
        self.tock_work(work);
        self.type3_glyph = saved_type3_glyph;
        self.suppress_color_ops = saved_suppress_color_ops;
        self.current = saved_state;
        self.oc_hidden_from = saved_oc_hidden;
        self.text_sink = saved_text_sink;
        self.span_streams = saved_span_streams;
        self.rule_sink = saved_rule_sink;
        let mask_dl = std::mem::replace(&mut self.display_list, saved_dl);

        // /BC components live in the group's /CS; the mean is a serviceable
        // luminosity approximation (gray and achromatic RGB are exact).
        let backdrop_luma = sm
            .get_array("BC")
            .ok()
            .map(|arr| {
                let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
                if v.is_empty() {
                    0.0
                } else {
                    (v.iter().sum::<f64>() / v.len() as f64).clamp(0.0, 1.0) as f32
                }
            })
            .unwrap_or(0.0);

        let transfer = sm.get("TR").and_then(|o| {
            if matches!(&self.resolve_plain(o), PdfObject::Name(n) if n.as_str() == "Identity") {
                return None;
            }
            let f = self.parse_function(o)?;
            let mut lut = [0u8; 256];
            for (i, slot) in lut.iter_mut().enumerate() {
                let out = f
                    .eval(&[i as f64 / 255.0])
                    .and_then(|v| v.first().copied())
                    .unwrap_or(i as f64 / 255.0);
                *slot = (out.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            Some(std::sync::Arc::new(lut))
        });

        Some(SoftMask {
            kind,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma,
            transfer,
        })
    }

    /// Build a [`SoftMask`] from a pre-resolved Type 3 `/ExtGState /SMask`
    /// entry during standalone CharProc replay (without a live `PdfFile`).
    fn build_type3_soft_mask(
        &mut self,
        is_alpha: bool,
        group: &zpdf_font::Type3FormXObject,
        backdrop_luma: f32,
        transfer_func: Option<&std::sync::Arc<zpdf_font::Type3Function>>,
    ) -> Option<SoftMask> {
        let kind = if is_alpha {
            SoftMaskKind::Alpha
        } else {
            SoftMaskKind::Luminosity
        };
        let mut mask_page_rect = self.display_list.page_rect.normalize();
        if let Ok(arr) = group.dict.get_array("BBox") {
            let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
            if v.len() == 4 && v.iter().all(|c| c.is_finite()) {
                let form_matrix = group
                    .dict
                    .get_array("Matrix")
                    .ok()
                    .and_then(|a| {
                        let m: Vec<f64> = a.iter().filter_map(|o| o.as_f64().ok()).collect();
                        (m.len() == 6).then(|| Matrix::new(m[0], m[1], m[2], m[3], m[4], m[5]))
                    })
                    .unwrap_or_else(Matrix::identity);
                let combined = self.current.ctm.concat(&form_matrix);
                let mut bbox_path = Path::new();
                bbox_path.rect(Rect::new(
                    v[0].min(v[2]),
                    v[1].min(v[3]),
                    v[0].max(v[2]),
                    v[1].max(v[3]),
                ));
                let mapped = bbox_path.map_points(|p| p.transform(&combined));
                if let Some(b) = Self::path_bounds(&mapped) {
                    mask_page_rect = Rect::new(
                        mask_page_rect.x0.min(b.x0),
                        mask_page_rect.y0.min(b.y0),
                        mask_page_rect.x1.max(b.x1),
                        mask_page_rect.y1.max(b.y1),
                    );
                }
            }
        }
        let saved_dl = std::mem::replace(&mut self.display_list, DisplayList::new(mask_page_rect));
        let saved_state = self.current.clone();
        let saved_oc_hidden = self.oc_hidden_from.take();
        let saved_text_sink = self.text_sink.take();
        let saved_span_streams = self.span_streams.take();
        let saved_rule_sink = self.rule_sink.take();
        // Marks inside the mask's transparency group emit concrete `Paint::Solid`
        // values and honor color operators even when invoked inside a `d1`
        // uncolored glyph so backends evaluate mask alpha and luminosity directly.
        let saved_type3_glyph = std::mem::replace(&mut self.type3_glyph, false);
        let saved_suppress_color_ops = std::mem::replace(&mut self.suppress_color_ops, false);
        self.current.soft_mask = None;
        self.current.blend_mode = BlendMode::Normal;
        self.current.fill_alpha = 1.0;
        self.current.stroke_alpha = 1.0;
        self.current.clip_bounds = None;
        self.current.clip_elision_bounds = None;

        let work = self.start_work(Some(Work::Form));
        self.do_type3_form_xobject(group);
        self.tock_work(work);

        self.type3_glyph = saved_type3_glyph;
        self.suppress_color_ops = saved_suppress_color_ops;
        self.current = saved_state;
        self.oc_hidden_from = saved_oc_hidden;
        self.text_sink = saved_text_sink;
        self.span_streams = saved_span_streams;
        self.rule_sink = saved_rule_sink;
        let mask_dl = std::mem::replace(&mut self.display_list, saved_dl);

        let transfer = transfer_func.and_then(|func| {
            let f = Self::parse_type3_function(func)?;
            let mut lut = [0u8; 256];
            for (i, slot) in lut.iter_mut().enumerate() {
                let out = f
                    .eval(&[i as f64 / 255.0])
                    .and_then(|v| v.first().copied())
                    .unwrap_or(i as f64 / 255.0);
                *slot = (out.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            Some(std::sync::Arc::new(lut))
        });

        Some(SoftMask {
            kind,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma,
            transfer,
        })
    }

    fn execute_do(&mut self, name: &str) {
        // A hidden draw does not need a resource. In strict mode, failures in
        // invisible layers must not reject otherwise complete visible output.
        if self.oc_suppressed() {
            self.type3_oc_suppressed_mark = true;
            return;
        }
        if self.file.is_none() {
            if let Some(xobj) = self.lookup_type3_xobject(name) {
                match xobj {
                    zpdf_font::Type3XObject::Image(img) => {
                        let work = self.start_work(Some(Work::Image));
                        self.do_type3_image_xobject(&img);
                        self.tock_work(work);
                    }
                    zpdf_font::Type3XObject::Form(form) => {
                        let work = self.start_work(Some(Work::Form));
                        self.do_type3_form_xobject(&form);
                        self.tock_work(work);
                    }
                }
                return;
            }
        }
        let file = match self.file {
            Some(f) => f,
            _ => return,
        };

        let xobj_id = match self.lookup_res(|r| r.xobjects.get(name).copied()) {
            Some(id) => id,
            None => {
                self.stats.diagnostics.missing_resources =
                    self.stats.diagnostics.missing_resources.saturating_add(1);
                tracing::warn!("XObject not found: {name}");
                return;
            }
        };

        let obj = match file.resolve_shared(xobj_id) {
            Ok(o) => o,
            Err(e) => {
                self.stats.diagnostics.record_object_failure(xobj_id, &e);
                tracing::warn!("failed to resolve XObject {name}: {e}");
                return;
            }
        };

        let stream = match obj.as_stream() {
            Ok(s) => s,
            Err(e) => {
                self.stats.diagnostics.record_object_failure(xobj_id, &e);
                return;
            }
        };

        // Hidden optional content: skip the XObject entirely (both the
        // surrounding BDC /OC state and the XObject's own /OC key).
        if let Some(oc) = stream.dict.get("OC") {
            if !self.oc_object_visible(oc, 0) {
                self.type3_oc_suppressed_mark = true;
                return;
            }
        }

        let subtype = stream.dict.get_name("Subtype").unwrap_or_default();

        match subtype {
            "Image" => {
                let work = self.start_work(Some(Work::Image));
                self.do_image_xobject(xobj_id, stream);
                self.tock_work(work);
            }
            "Form" => {
                let work = self.start_work(Some(Work::Form));
                self.do_form_xobject(Some(xobj_id), stream, file);
                self.tock_work(work);
            }
            _ => {
                self.stats.diagnostics.unsupported_features = self
                    .stats
                    .diagnostics
                    .unsupported_features
                    .saturating_add(1);
                tracing::warn!("unknown XObject subtype: {subtype}");
            }
        }
    }

    fn do_image_xobject(&mut self, obj_id: zpdf_core::ObjectId, stream: &zpdf_core::PdfStream) {
        if self.skip_images {
            return;
        }
        // Outside Type 3 glyphs, omit Image XObjects when no `ImageCache` is
        // attached. Inside an active Type 3 glyph (including helper Forms),
        // embed decoded pixels directly on `DrawInlineImage` so stencil and
        // bitmap XObject glyphs survive fonts-only rendering.
        let embed_type3_inline = self.image_cache.is_none() && !self.active_type3_glyphs.is_empty();
        if self.image_cache.is_none() && !embed_type3_inline {
            return;
        }
        let transform = self.current.ctm;

        if embed_type3_inline {
            if let Some(&(width, height)) = self.image_mask_sizes.get(&obj_id) {
                if cs_produces_no_marks(&self.current.fill_cs) {
                    return;
                }
                let footprint = self.image_footprint(&transform, width, height);
                let key = (obj_id, Some(self.fill_rgb_u8()), footprint);
                if let Some(inline) = self.type3_inline_xobject_cache.get(&key).cloned() {
                    self.emit_inline_image_draw(inline, transform);
                    return;
                }
                if self
                    .type3_rejected_xobject_footprints
                    .contains(&(obj_id, footprint))
                {
                    self.type3_image_dropped = true;
                    return;
                }
            } else if let Some(&(width, height)) = self.type3_inline_xobject_sizes.get(&obj_id) {
                let footprint = self.image_footprint(&transform, width, height);
                let key = (obj_id, None, footprint);
                if let Some(inline) = self.type3_inline_xobject_cache.get(&key).cloned() {
                    self.emit_inline_image_draw(inline, transform);
                    return;
                }
                if self
                    .type3_rejected_xobject_footprints
                    .contains(&(obj_id, footprint))
                {
                    self.type3_image_dropped = true;
                    return;
                }
            }
        } else {
            // Fast path: this image object is already stored at the size this
            // draw uses — re-emit it under the current CTM/alpha without
            // re-decoding (huge win for repeated symbols).
            if let Some(image_id) = self.stored_image(obj_id, &transform) {
                self.emit_image_draw(image_id, transform, false);
                return;
            }
            if self.rejected_image_objects.contains(&obj_id) {
                self.type3_image_dropped = true;
                return;
            }
            if let Some(&(width, height)) = self.image_mask_sizes.get(&obj_id) {
                if cs_produces_no_marks(&self.current.fill_cs) {
                    return;
                }
                let (width, height) = self.image_footprint(&transform, width, height);
                let key = (obj_id, self.fill_rgb_u8(), width, height);
                if let Some(&image_id) = self.image_mask_cache.get(&key) {
                    self.emit_image_draw(image_id, transform, true);
                    return;
                }
            }
            // A new footprint of a known image comes from its retained source
            // rather than from decoding the stream again.
            match self.store_from_retained(obj_id, &transform) {
                Stored::Image(image_id) => {
                    self.emit_image_draw(image_id, transform, false);
                    return;
                }
                Stored::Skipped => {
                    self.type3_image_dropped = true;
                    self.rejected_image_objects.insert(obj_id);
                    return;
                }
                Stored::NeedsDecode => {}
            }
        }

        let file = match self.file {
            Some(f) => f,
            None => return,
        };

        let decoded_data = match file.resolve_stream_data_shared(obj_id) {
            Ok(d) => d,
            Err(e) => {
                self.type3_image_dropped = true;
                self.stats.diagnostics.record_object_failure(obj_id, &e);
                tracing::warn!("failed to decode image stream: {e}");
                return;
            }
        };

        // Image metadata keys may be indirect references; resolve them so e.g.
        // an indirect /Decode does not silently invert an /ImageMask stencil.
        let image_dict = resolve_image_metadata(file, &stream.dict);
        // An /ImageMask stencil paints in the current fill colour, so a `/None`
        // (no-marks) fill colour space suppresses it like any other fill paint.
        if cs_produces_no_marks(&self.current.fill_cs)
            && matches!(image_dict.get("ImageMask"), Some(PdfObject::Bool(true)))
        {
            return;
        }
        // An image's /Intent overrides the graphics-state rendering intent. A
        // second decode keeps the first decode's intent, which every draw of
        // a cached copy would use.
        let intent = match self.image_sources.get(&obj_id) {
            Some(source) => source.intent,
            None => image_dict
                .get_name("Intent")
                .ok()
                .map(zpdf_color::RenderIntent::from_pdf_name)
                .unwrap_or(self.current.rendering_intent),
        };
        let colorspace = resolve_image_colorspace(
            Some(file),
            self.icc_cache.as_deref_mut(),
            image_dict.get("ColorSpace"),
            0,
            intent,
        );
        let colorspace = self.colormanage_cmyk_image(colorspace);
        let smask_ref = stream.dict.get_ref("SMask").ok();
        // On the first minified draw, opaque Gray/RGB scans need only one
        // converted source row, not a full-resolution RGBA allocation. Later
        // sizes retain the existing two-decode/source-retention policy.
        if !embed_type3_inline
            && self.image_downscale_scale > 0.0
            && !self.image_sources.contains_key(&obj_id)
        {
            match zpdf_image::decode_scan_at_footprint(
                &decoded_data,
                &image_dict,
                colorspace.as_ref(),
                &transform,
                self.image_downscale_scale,
                &self.parse_limits,
            ) {
                Ok(Some(scan)) => {
                    let size = (scan.image.width, scan.image.height);
                    self.image_sources.insert(
                        obj_id,
                        ImageSource {
                            width: scan.source_width,
                            height: scan.source_height,
                            bytes: rgba_bytes((scan.source_width, scan.source_height)),
                            intent,
                            decodes: 1,
                            filtered_bytes: 0,
                            unfiltered: false,
                            retained: None,
                        },
                    );
                    let bytes = scan.image.data.capacity() as u64;
                    if let Some(image_id) = self.cache_filtered(obj_id, size, scan.image) {
                        self.emit_image_draw(image_id, transform, false);
                    } else {
                        self.warn_image_dropped(bytes);
                        self.rejected_image_objects.insert(obj_id);
                    }
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    self.stats.diagnostics.record_object_failure(obj_id, &e);
                    tracing::warn!("failed to decode scan: {e}");
                    return;
                }
            }
        }
        // A soft mask's `/Matte` is a colour in this space.
        let matte_colorspace = smask_ref.and_then(|_| colorspace.clone());
        let mut image = match zpdf_image::decode_image_xobject_resolved_with_limits(
            &decoded_data,
            &image_dict,
            self.fill_rgb_u8(),
            colorspace,
            &self.parse_limits,
        ) {
            Ok(img) => img,
            Err(e) => {
                self.type3_image_dropped = true;
                self.stats.diagnostics.record_object_failure(obj_id, &e);
                tracing::warn!("failed to decode image: {e}");
                return;
            }
        };

        // /SMask (soft mask) decodes through the full image path — filters,
        // predictors, any bpc, /Decode — and its gray level becomes the alpha.
        // /Mask as a stream ref is a 1-bpc stencil; /Mask colour-key arrays
        // were already handled inside decode_image_xobject_resolved.
        if let Some(smask_ref) = smask_ref {
            fold_soft_mask(
                &mut image,
                smask_ref,
                matte_colorspace.as_ref(),
                file,
                &self.parse_limits,
                &mut self.stats.diagnostics,
            );
        } else if let Ok(mask_ref) = stream.dict.get_ref("Mask") {
            fold_stencil_mask(
                &mut image,
                mask_ref,
                file,
                &self.parse_limits,
                &mut self.stats.diagnostics,
            );
        }

        if embed_type3_inline {
            let is_mask = image.is_image_mask;
            let (src_w, src_h) = (image.width, image.height);
            if is_mask {
                self.image_mask_sizes.insert(obj_id, (src_w, src_h));
            } else {
                self.type3_inline_xobject_sizes
                    .insert(obj_id, (src_w, src_h));
            }
            let footprint = self.image_footprint(&transform, src_w, src_h);
            let color_key = is_mask.then(|| self.fill_rgb_u8());
            if let Some(inline) = self.emit_draw_inline_image(image) {
                self.type3_inline_xobject_cache
                    .insert((obj_id, color_key, footprint), inline);
            } else {
                self.type3_rejected_xobject_footprints
                    .insert((obj_id, footprint));
            }
            return;
        }

        // `/ImageMask` stencils bake in the fill colour, so they are cached
        // per colour, apart from the stateless images below.
        if image.is_image_mask {
            self.image_mask_sizes
                .insert(obj_id, (image.width, image.height));
            let image = self.fit_image_to_footprint(image, &transform);
            let key = (obj_id, self.fill_rgb_u8(), image.width, image.height);
            let Some(image_id) = self.admit_image(image) else {
                self.rejected_image_objects.insert(obj_id);
                return;
            };
            self.image_mask_cache.insert(key, image_id);
            self.emit_image_draw(image_id, transform, true);
            return;
        }
        // Insert once and remember the id so repeat draws skip the decode above.
        let Some(image_id) = self.store_decoded_image(obj_id, image, &transform, intent) else {
            self.rejected_image_objects.insert(obj_id);
            return;
        };
        self.emit_image_draw(image_id, transform, false);
    }

    /// The size an image of `width`×`height` samples is stored at for a draw
    /// under `transform`: with [`Self::with_image_downscale`], the size the CPU
    /// backend would box-filter the source to; otherwise the decoded size.
    ///
    /// A filtered copy is used only if the backend samples it directly. When
    /// an axis spans under half a device pixel the footprint keeps one sample,
    /// the backend would apply its rule to that copy again, and the other
    /// axis can round one sample lower. Such a draw keeps the source size so
    /// that the backend filters once, from the source.
    fn image_footprint(&self, transform: &Matrix, width: u32, height: u32) -> (u32, u32) {
        let scale = self.image_downscale_scale;
        if scale > 0.0 {
            let fixed = zpdf_image::box_filter_size(transform, scale, width, height).filter(
                |&(width, height)| {
                    zpdf_image::box_filter_size(transform, scale, width, height).is_none()
                },
            );
            if let Some(size) = fixed {
                return size;
            }
        }
        (width, height)
    }

    /// Filter a freshly decoded image that is not cached to its footprint.
    fn fit_image_to_footprint(
        &self,
        image: zpdf_image::DecodedImage,
        transform: &Matrix,
    ) -> zpdf_image::DecodedImage {
        let (width, height) = self.image_footprint(transform, image.width, image.height);
        if (width, height) == (image.width, image.height) {
            image
        } else {
            zpdf_image::box_downscale(&image, width, height)
        }
    }

    /// The cached copy of an image XObject this draw can use: its footprint,
    /// or the source once the source is in the image cache.
    fn stored_image(&self, obj_id: ObjectId, transform: &Matrix) -> Option<zpdf_image::ImageId> {
        let source = self.image_sources.get(&obj_id)?;
        let (width, height) = self.image_footprint(transform, source.width, source.height);
        if let Some(&image_id) = self.image_obj_cache.get(&(obj_id, width, height)) {
            return Some(image_id);
        }
        if source.unfiltered {
            return self
                .image_obj_cache
                .get(&(obj_id, source.width, source.height))
                .copied();
        }
        None
    }

    /// Store a freshly decoded image XObject for this draw. The first decode
    /// keeps only a filtered copy. A second decode (another size was needed)
    /// also keeps the source: retained to filter later sizes when the byte
    /// limit has room for both, else cached unfiltered when that fits.
    fn store_decoded_image(
        &mut self,
        obj_id: ObjectId,
        image: zpdf_image::DecodedImage,
        transform: &Matrix,
        intent: zpdf_color::RenderIntent,
    ) -> Option<zpdf_image::ImageId> {
        let (width, height) = (image.width, image.height);
        let bytes = u64::try_from(image.data.capacity()).unwrap_or(u64::MAX);
        // Decoding is deterministic; a different size means the earlier
        // entry no longer describes this stream.
        let (decodes, filtered_bytes) = match self.image_sources.get(&obj_id) {
            Some(source) if (source.width, source.height) == (width, height) => {
                (source.decodes.saturating_add(1), source.filtered_bytes)
            }
            _ => (1, 0),
        };
        self.image_sources.insert(
            obj_id,
            ImageSource {
                width,
                height,
                bytes,
                intent,
                decodes,
                filtered_bytes,
                unfiltered: false,
                retained: None,
            },
        );
        let size = self.image_footprint(transform, width, height);
        if size == (width, height) || filtered_bytes.saturating_add(rgba_bytes(size)) > bytes {
            // An unminified draw, or the object's filtered copies would
            // outgrow its source: cache the source itself.
            return self.cache_unfiltered(obj_id, image);
        }
        let filtered = zpdf_image::box_downscale(&image, size.0, size.1);
        let filtered_len = u64::try_from(filtered.data.capacity()).unwrap_or(u64::MAX);
        if decodes == 1 {
            let image_id = self.cache_filtered(obj_id, size, filtered);
            if image_id.is_none() {
                self.warn_image_dropped(filtered_len);
            }
            return image_id;
        }
        if self.image_room(filtered_len.saturating_add(bytes)) {
            self.retained_image_bytes = self.retained_image_bytes.saturating_add(bytes);
            if let Some(source) = self.image_sources.get_mut(&obj_id) {
                source.retained = Some(image);
            }
            return self
                .cache_filtered(obj_id, size, filtered)
                .or_else(|| self.cache_retained(obj_id));
        }
        // No room to keep the source beside a filtered copy: cache it
        // unfiltered if it fits, else keep just this footprint. Later sizes
        // are then skipped rather than decoded a third time.
        if self.image_room(bytes) {
            return self.cache_unfiltered(obj_id, image);
        }
        let image_id = self.cache_filtered(obj_id, size, filtered);
        if image_id.is_none() {
            self.warn_image_dropped(filtered_len);
        }
        image_id
    }

    /// Store this draw's footprint from the object's retained source.
    fn store_from_retained(&mut self, obj_id: ObjectId, transform: &Matrix) -> Stored {
        let Some(source) = self.image_sources.get(&obj_id) else {
            return Stored::NeedsDecode;
        };
        let Some(pixels) = &source.retained else {
            if source.decodes >= 2 {
                // After its second decode the source fit the byte limit
                // neither retained nor cached: draw only the cached sizes.
                tracing::warn!(
                    "image {obj_id:?}: no room to keep the source for another size; skipping draw"
                );
                self.stats.diagnostics.resource_limit_reached = true;
                return Stored::Skipped;
            }
            return Stored::NeedsDecode;
        };
        let full = (source.width, source.height);
        let size = self.image_footprint(transform, full.0, full.1);
        if size != full && source.filtered_bytes.saturating_add(rgba_bytes(size)) <= source.bytes {
            let filtered = zpdf_image::box_downscale(pixels, size.0, size.1);
            if let Some(image_id) = self.cache_filtered(obj_id, size, filtered) {
                return Stored::Image(image_id);
            }
        }
        // An unminified draw, filtered copies that would outgrow the source,
        // or no room for another copy: from now on the object draws its
        // source, which the CPU backend filters on draw.
        match self.cache_retained(obj_id) {
            Some(image_id) => Stored::Image(image_id),
            None => Stored::Skipped,
        }
    }

    /// Admit a filtered copy of an image XObject.
    fn cache_filtered(
        &mut self,
        obj_id: ObjectId,
        size: (u32, u32),
        filtered: zpdf_image::DecodedImage,
    ) -> Option<zpdf_image::ImageId> {
        let bytes = u64::try_from(filtered.data.capacity()).unwrap_or(u64::MAX);
        let image_id = self.insert_image(filtered)?;
        if let Some(source) = self.image_sources.get_mut(&obj_id) {
            source.filtered_bytes = source.filtered_bytes.saturating_add(bytes);
        }
        self.image_obj_cache
            .insert((obj_id, size.0, size.1), image_id);
        Some(image_id)
    }

    /// Move the retained source into the image cache. Its bytes were already
    /// counted against the limit, so only an exhausted id space can fail.
    fn cache_retained(&mut self, obj_id: ObjectId) -> Option<zpdf_image::ImageId> {
        let image = self.image_sources.get_mut(&obj_id)?.retained.take()?;
        self.retained_image_bytes = self
            .retained_image_bytes
            .saturating_sub(u64::try_from(image.data.capacity()).unwrap_or(u64::MAX));
        self.cache_unfiltered(obj_id, image)
    }

    /// Cache the source; later draws of the object without a cached
    /// footprint use it.
    fn cache_unfiltered(
        &mut self,
        obj_id: ObjectId,
        image: zpdf_image::DecodedImage,
    ) -> Option<zpdf_image::ImageId> {
        let size = (image.width, image.height);
        let image_id = self.admit_image(image)?;
        if let Some(source) = self.image_sources.get_mut(&obj_id) {
            source.unfiltered = true;
        }
        self.image_obj_cache
            .insert((obj_id, size.0, size.1), image_id);
        Some(image_id)
    }

    /// Count `bytes` of decoded content against [`MAX_CONTENT_CACHE_BYTES`];
    /// false (nothing counted) when they don't fit.
    fn reserve_content_cache(&mut self, bytes: usize) -> bool {
        let total = self.content_cache_bytes.saturating_add(bytes);
        if total > MAX_CONTENT_CACHE_BYTES {
            return false;
        }
        self.content_cache_bytes = total;
        true
    }

    /// Whether `bytes` more decoded pixels fit the image byte limit beside
    /// the cache and the retained sources.
    fn image_room(&self, bytes: u64) -> bool {
        self.image_cache
            .as_ref()
            .map_or(0, |cache| cache.bytes_used())
            .saturating_add(self.retained_image_bytes)
            .saturating_add(self.type3_inline_fallback_bytes)
            .saturating_add(bytes)
            <= self.max_image_cache_bytes
    }

    /// Paint an `/ImageMask true` stencil with the active fill pattern by
    /// rendering `pat` inside a blend group modulated by a `SoftMaskKind::Alpha`
    /// carrying the stencil coverage. Returns `false` if the stencil transform
    /// is degenerate or tiling replication cannot proceed (caller falls back to
    /// the solid fill approximation).
    fn emit_patterned_image_mask(
        &mut self,
        mask_cmd: RenderCommand,
        transform: Matrix,
        pat: &PatternPaint,
    ) -> bool {
        if self.oc_suppressed() {
            self.type3_oc_suppressed_mark = true;
            return true;
        }
        let corners = [
            Point::new(0.0, 0.0).transform(&transform),
            Point::new(1.0, 0.0).transform(&transform),
            Point::new(0.0, 1.0).transform(&transform),
            Point::new(1.0, 1.0).transform(&transform),
        ];
        let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
        let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for c in &corners {
            x0 = x0.min(c.x);
            y0 = y0.min(c.y);
            x1 = x1.max(c.x);
            y1 = y1.max(c.y);
        }
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite())
            || x1 <= x0
            || y1 <= y0
        {
            return false;
        }
        let bounds = Rect::new(x0, y0, x1, y1);
        let mut mask_dl = DisplayList::new(self.display_list.page_rect);
        mask_dl.push(mask_cmd);
        let stencil_mask = SoftMask {
            kind: SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut tiling_paint: Option<std::sync::Arc<TilingPattern>> = None;
        let non_isolated = match pat {
            PatternPaint::Tiling { def, base_ctm } => {
                let Some(tiling) =
                    self.build_tiling_pattern_paint(def, *base_ctm, self.current.fill_color)
                else {
                    return false;
                };
                let dep = tiling.is_backdrop_dependent();
                tiling_paint = Some(tiling);
                dep
            }
            PatternPaint::Shading(_) => false,
        };

        let cmd_mark = self.display_list.commands.len();
        let has_outer_group =
            self.current.blend_mode != BlendMode::Normal || self.current.soft_mask.is_some();
        if has_outer_group {
            self.display_list.push(RenderCommand::PushBlendGroup {
                blend_mode: self.current.blend_mode,
                // Keep the live page backdrop available through the outer
                // group when the tiling pattern depends on the backdrop
                // (ISO 32000-1 §11.6.7).
                isolated: !non_isolated,
                knockout: false,
                bounds: self.display_list.page_rect,
                alpha: 1.0,
                mask: self.current.soft_mask.clone(),
            });
        }
        self.display_list.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            // A backdrop-dependent tiling pattern painted through an
            // `/ImageMask true` stencil must evaluate against the live page
            // backdrop rather than transparent black, applying stencil
            // coverage and `fill_alpha` once on `PopBlendGroup`.
            isolated: !non_isolated,
            knockout: false,
            bounds: self.display_list.page_rect,
            alpha: self.current.fill_alpha,
            mask: Some(stencil_mask),
        });

        let painted = if let Some(tiling) = tiling_paint {
            let mut rect_path = Path::new();
            rect_path.rect(Rect::new(
                bounds.x0.floor() - 1.0,
                bounds.y0.floor() - 1.0,
                bounds.x1.ceil() + 1.0,
                bounds.y1.ceil() + 1.0,
            ));
            self.display_list.push(RenderCommand::FillPath {
                path: rect_path,
                rule: FillRule::NonZero,
                paint: Paint::Tiling(tiling),
                alpha: 1.0,
                overprint: None,
            });
            true
        } else {
            let saved_blend = std::mem::replace(&mut self.current.blend_mode, BlendMode::Normal);
            let saved_soft_mask = self.current.soft_mask.take();
            let saved_fill_alpha = std::mem::replace(&mut self.current.fill_alpha, 1.0);
            let saved_stroke_alpha = std::mem::replace(&mut self.current.stroke_alpha, 1.0);
            let painted = self.paint_pattern_in_region(pat, bounds);
            self.current.blend_mode = saved_blend;
            self.current.soft_mask = saved_soft_mask;
            self.current.fill_alpha = saved_fill_alpha;
            self.current.stroke_alpha = saved_stroke_alpha;
            painted
        };

        if painted {
            self.display_list.push(RenderCommand::PopBlendGroup);
            if has_outer_group {
                self.display_list.push(RenderCommand::PopBlendGroup);
            }
            true
        } else {
            self.display_list.truncate_commands(cmd_mark);
            false
        }
    }

    fn emit_image_draw(
        &mut self,
        image_id: zpdf_image::ImageId,
        transform: Matrix,
        is_image_mask: bool,
    ) {
        if !self.active_type3_glyphs.is_empty()
            && (!is_image_mask
                || self.current.fill_pattern.is_some()
                || self
                    .type3_caller_fill
                    .is_some_and(|caller| !colors_close(self.current.fill_color, caller)))
        {
            self.type3_local_image_paint = true;
        }
        if is_image_mask {
            if let Some(pat) = self.current.fill_pattern.clone() {
                let mask_cmd = RenderCommand::DrawImage(ImageDraw {
                    image_id,
                    transform,
                    alpha: 1.0,
                    is_image_mask: true,
                });
                if self.emit_patterned_image_mask(mask_cmd, transform, &pat) {
                    return;
                }
            }
        }
        self.emit_painted(RenderCommand::DrawImage(ImageDraw {
            image_id,
            transform,
            alpha: self.current.fill_alpha,
            is_image_mask,
        }));
    }

    fn emit_inline_image_draw(
        &mut self,
        image: std::sync::Arc<zpdf_display_list::InlineImage>,
        transform: Matrix,
    ) {
        if !self.active_type3_glyphs.is_empty()
            && (!image.is_image_mask
                || self.current.fill_pattern.is_some()
                || self
                    .type3_caller_fill
                    .is_some_and(|caller| !colors_close(self.current.fill_color, caller)))
        {
            self.type3_local_image_paint = true;
        }
        if image.is_image_mask {
            if let Some(pat) = self.current.fill_pattern.clone() {
                let mask_cmd = RenderCommand::DrawInlineImage(zpdf_display_list::InlineImageDraw {
                    image: std::sync::Arc::clone(&image),
                    transform,
                    alpha: 1.0,
                });
                if self.emit_patterned_image_mask(mask_cmd, transform, &pat) {
                    return;
                }
            }
        }
        self.emit_painted(RenderCommand::DrawInlineImage(
            zpdf_display_list::InlineImageDraw {
                image,
                transform,
                alpha: self.current.fill_alpha,
            },
        ));
    }

    fn do_inline_image(&mut self, dict: zpdf_core::PdfDict, data: Vec<u8>) {
        // BI ... EI is one operation in every content loop. Charge it even
        // when rendering is skipped (for example a page without an image
        // cache), just as execute_operator charges suppressed operators.
        self.ops_executed = self.ops_executed.saturating_add(1);

        // Allocate the per-glyph inline-image ordinal in source order before
        // visibility or cache guards so an OC-hidden image never shifts a later
        // visible image into an earlier cache slot.
        let type3_slot = self
            .type3_inline_ctx
            .as_mut()
            .map(|(font_id, glyph_id, ordinal)| {
                let idx = *ordinal;
                *ordinal = ordinal.saturating_add(1);
                (*font_id, *glyph_id, idx)
            });
        if self.oc_suppressed() {
            self.type3_oc_suppressed_mark = true;
            return;
        }
        let mut norm = normalize_inline_image_dict(&dict);
        let is_mask = matches!(norm.get("ImageMask"), Some(PdfObject::Bool(true)));
        // An inline /ImageMask stencil paints in the current fill colour, so a
        // `/None` (no-marks) fill colour space suppresses it.
        if cs_produces_no_marks(&self.current.fill_cs) && is_mask {
            return;
        }
        if self.skip_images {
            return;
        }
        // Outside Type 3 glyphs, omit inline images when the caller supplied no
        // `ImageCache`. Inside a Type 3 glyph (including helper Forms it calls),
        // embed decoded pixels directly on `DrawInlineImage` so bitmap glyphs
        // and mixed Form+stencil glyphs preserve every mark in paint order.
        let embed_type3_inline = self.image_cache.is_none()
            && (type3_slot.is_some() || !self.active_type3_glyphs.is_empty());
        if self.image_cache.is_none() && !embed_type3_inline {
            return;
        }
        let intent = norm
            .get_name("Intent")
            .ok()
            .map(zpdf_color::RenderIntent::from_pdf_name)
            .unwrap_or(self.current.rendering_intent);
        // Resolve any named non-device `/ColorSpace` resource BEFORE checking
        // the Type 3 inline-image cache so placements inheriting different
        // colour spaces or rendering intents never alias each other's pixels.
        let mut resolved_named_cs: Option<Option<ObjectId>> = None;
        if !is_mask {
            if let Some(PdfObject::Name(n)) = norm.get("ColorSpace") {
                let cs_name = n.0.clone();
                if !matches!(
                    cs_name.as_str(),
                    "DeviceGray" | "G" | "DeviceRGB" | "RGB" | "DeviceCMYK" | "CMYK"
                ) {
                    if let Some(obj) = self.lookup_color_space_resource(&cs_name) {
                        let ref_id = match &obj {
                            PdfObject::Ref(id) => Some(*id),
                            _ => None,
                        };
                        norm.0.insert(zpdf_core::PdfName("ColorSpace".into()), obj);
                        resolved_named_cs = Some(ref_id);
                    } else {
                        resolved_named_cs = Some(None);
                    }
                }
            }
        }
        let color_key = if is_mask {
            Some(Type3InlineColorKey::Mask(self.fill_rgb_u8()))
        } else {
            match resolved_named_cs {
                // Named resource resolved to a concrete indirect object id.
                Some(Some(cs_id)) => Some(Type3InlineColorKey::Color {
                    cs_ref: Some(cs_id),
                    intent,
                }),
                // Named resource resolved to an inline object or was missing:
                // skip cross-placement pixel reuse because enclosing scopes can
                // bind different inline palettes under the same resource name.
                Some(None) => None,
                // Self-contained device/array/absent `/ColorSpace`.
                None => Some(Type3InlineColorKey::Color {
                    cs_ref: None,
                    intent,
                }),
            }
        };
        let type3_cache_key =
            if let (Some((font_id, glyph_id, idx)), Some(color_key)) = (type3_slot, color_key) {
                let w = norm
                    .get_i64("Width")
                    .ok()
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0);
                let h = norm
                    .get_i64("Height")
                    .ok()
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0);
                let footprint = self.image_footprint(&self.current.ctm, w, h);
                let key = (font_id, glyph_id, idx, color_key, footprint);
                if embed_type3_inline {
                    if let Some(image) = self.type3_inline_fallback_cache.get(&key) {
                        self.emit_inline_image_draw(std::sync::Arc::clone(image), self.current.ctm);
                        return;
                    }
                } else if let Some(&(image_id, cached_is_mask)) =
                    self.type3_inline_image_cache.get(&key)
                {
                    self.emit_image_draw(image_id, self.current.ctm, cached_is_mask);
                    return;
                }
                Some(key)
            } else {
                None
            };
        let decoded = match zpdf_parser::filters::decode_stream_borrowed_with_limits(
            &data,
            &norm,
            &self.parse_limits,
        ) {
            Ok(d) => d,
            Err(e) => {
                self.type3_image_dropped = true;
                self.stats.diagnostics.record_decode_failure(&e);
                tracing::warn!("inline image: filter decode failed: {e}");
                return;
            }
        };
        let colorspace = resolve_image_colorspace(
            self.file,
            self.icc_cache.as_deref_mut(),
            norm.get("ColorSpace"),
            0,
            intent,
        );
        let colorspace = self.colormanage_cmyk_image(colorspace);
        match zpdf_image::decode_image_xobject_resolved_with_limits(
            &decoded,
            &norm,
            self.fill_rgb_u8(),
            colorspace,
            &self.parse_limits,
        ) {
            Ok(img) => {
                if embed_type3_inline {
                    if let Some(inline) = self.emit_draw_inline_image(img) {
                        if let Some(key) = type3_cache_key {
                            self.type3_inline_fallback_cache.insert(key, inline);
                        }
                    }
                } else if let Some(image_id) = self.emit_draw_image(img) {
                    if let Some(key) = type3_cache_key {
                        self.type3_inline_image_cache
                            .insert(key, (image_id, is_mask));
                    }
                }
            }
            Err(e) => {
                self.type3_image_dropped = true;
                self.stats.diagnostics.record_decode_failure(&e);
                tracing::warn!("inline image: {e}");
            }
        }
    }

    /// Current fill colour as 8-bit RGB, for painting `/ImageMask` stencils.
    fn fill_rgb_u8(&self) -> [u8; 3] {
        let c = self.current.fill_color;
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [to_u8(c.r), to_u8(c.g), to_u8(c.b)]
    }

    fn do_type3_image_xobject(&mut self, img: &zpdf_font::Type3ImageXObject) {
        if self.image_cache.is_none() || self.oc_suppressed() {
            return;
        }
        if cs_produces_no_marks(&self.current.fill_cs)
            && matches!(img.dict.get("ImageMask"), Some(PdfObject::Bool(true)))
        {
            return;
        }
        let mut owned_dict = None;
        if let Some(PdfObject::Name(n)) = img.dict.get("ColorSpace") {
            let cs_name = n.0.as_str();
            if !matches!(
                cs_name,
                "DeviceGray" | "G" | "DeviceRGB" | "RGB" | "DeviceCMYK" | "CMYK"
            ) {
                if let Some(obj) = self.lookup_type3_colorspace(cs_name) {
                    let mut d = img.dict.clone();
                    d.0.insert(zpdf_core::PdfName("ColorSpace".into()), obj);
                    owned_dict = Some(d);
                }
            }
        }
        let dict = owned_dict.as_ref().unwrap_or(&img.dict);
        let intent = dict
            .get_name("Intent")
            .ok()
            .map(zpdf_color::RenderIntent::from_pdf_name)
            .unwrap_or(self.current.rendering_intent);
        let colorspace = resolve_image_colorspace(
            None,
            self.icc_cache.as_deref_mut(),
            dict.get("ColorSpace"),
            0,
            intent,
        );
        let colorspace = self.colormanage_cmyk_image(colorspace);
        let matte_colorspace = img.smask.as_ref().and_then(|_| colorspace.clone());
        let mut decoded = match zpdf_image::decode_image_xobject_resolved_with_limits(
            &img.data,
            dict,
            self.fill_rgb_u8(),
            colorspace,
            &self.parse_limits,
        ) {
            Ok(decoded) => decoded,
            Err(e) => {
                self.stats.diagnostics.record_decode_failure(&e);
                tracing::warn!("Type3 image XObject decode failed: {e}");
                return;
            }
        };
        if let Some(sm) = &img.smask {
            if let Ok(mask) = zpdf_image::decode_image_xobject_resolved_with_limits(
                &sm.data,
                &sm.dict,
                [0, 0, 0],
                Some(zpdf_image::ResolvedColorSpace::Gray),
                &self.parse_limits,
            ) {
                zpdf_image::apply_smask_image_with_matte(
                    &mut decoded,
                    &mask,
                    zpdf_image::smask_matte(&sm.dict).as_deref(),
                    matte_colorspace.as_ref(),
                );
            }
        } else if let Some(ms) = &img.mask_stream {
            let width = ms.dict.get_i64("Width").unwrap_or(0) as u32;
            let height = ms.dict.get_i64("Height").unwrap_or(0) as u32;
            if width > 0
                && height > 0
                && u64::from(width) * u64::from(height) <= self.parse_limits.max_image_pixels
            {
                let invert = zpdf_image::mask_decode_inverts(&ms.dict);
                zpdf_image::apply_stencil_mask_with_limits(
                    &mut decoded,
                    &ms.data,
                    width,
                    height,
                    invert,
                    &self.parse_limits,
                );
            }
        }
        self.emit_draw_image(decoded);
    }

    fn do_type3_form_xobject(&mut self, form: &zpdf_font::Type3FormXObject) {
        const MAX_FORM_DEPTH: u32 = 16;
        if self.form_depth >= MAX_FORM_DEPTH || self.over_budget() {
            return;
        }
        let form_matrix = if let Ok(arr) = form.dict.get_array("Matrix") {
            let vals: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
            if vals.len() == 6 {
                Matrix::new(vals[0], vals[1], vals[2], vals[3], vals[4], vals[5])
            } else {
                Matrix::identity()
            }
        } else {
            Matrix::identity()
        };
        let pushed_res = if let Some(res) = &form.resources {
            self.type3_resource_stack.push(std::sync::Arc::clone(res));
            true
        } else {
            false
        };
        let depth_floor = self.state_stack.len();
        self.state_stack.push(self.current.clone());
        let saved_floor = self.state_floor;
        self.state_floor = depth_floor + 1;
        let saved_operand_stack = std::mem::take(&mut self.operand_stack);
        self.current.ctm = self.current.ctm.concat(&form_matrix);
        self.current.clip_depth = 0;

        let mut bbox_clip = false;
        let mut bbox_bounds: Option<Rect> = None;
        if let Ok(arr) = form.dict.get_array("BBox") {
            let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
            if v.len() == 4 {
                let (x0, y0) = (v[0].min(v[2]), v[1].min(v[3]));
                let (x1, y1) = (v[0].max(v[2]), v[1].max(v[3]));
                if x1 > x0 && y1 > y0 {
                    let mut bbox_path = Path::new();
                    bbox_path.rect(Rect::new(x0, y0, x1, y1));
                    let page_path = self.transform_path_to_page_space(&bbox_path);
                    bbox_bounds = Self::path_bounds(&page_path);
                    self.intersect_clip_bounds(bbox_bounds);
                    self.display_list.push(RenderCommand::PushClip {
                        path: page_path,
                        rule: FillRule::NonZero,
                    });
                    bbox_clip = true;
                }
            }
        }
        let group = form
            .dict
            .get("Group")
            .and_then(|o| o.as_dict().ok())
            .filter(|g| matches!(g.get_name("S"), Ok("Transparency")));
        let in_group = group.is_some();
        if let Some(g) = group {
            let flag = |key| matches!(g.get(key), Some(PdfObject::Bool(true)));
            self.display_list.push(RenderCommand::PushBlendGroup {
                blend_mode: self.current.blend_mode,
                isolated: flag("I"),
                knockout: flag("K"),
                bounds: bbox_bounds.unwrap_or(self.display_list.page_rect),
                alpha: self.current.fill_alpha,
                mask: self.current.soft_mask.take(),
            });
            self.current.blend_mode = BlendMode::Normal;
            self.current.fill_alpha = 1.0;
            self.current.stroke_alpha = 1.0;
        }

        self.form_depth += 1;
        let outer_clip = self.pending_clip.take();
        let mut tokenizer = ContentTokenizer::new(&form.data);
        while let Some(token) = tokenizer.next_borrowed_recovering() {
            if self.over_budget() {
                break;
            }
            match token {
                ContentToken::Operand(obj) => {
                    if !self.push_operand(obj) {
                        break;
                    }
                }
                ContentToken::Operator(op) => {
                    self.execute_operator(&op);
                    self.operand_stack.clear();
                }
                ContentToken::InlineImage { dict, data } => {
                    self.do_inline_image(dict, data);
                    self.operand_stack.clear();
                }
            }
        }
        self.pending_clip = outer_clip;
        self.form_depth -= 1;

        while self.state_stack.len() > depth_floor + 1 {
            for _ in 0..self.current.clip_depth {
                self.display_list.push(RenderCommand::PopClip);
            }
            if let Some(state) = self.state_stack.pop() {
                self.current = state;
            }
        }
        for _ in 0..self.current.clip_depth {
            self.display_list.push(RenderCommand::PopClip);
        }
        if let Some(state) = self.state_stack.pop() {
            self.current = state;
        }
        if in_group {
            self.display_list.push(RenderCommand::PopBlendGroup);
        }
        if bbox_clip {
            self.display_list.push(RenderCommand::PopClip);
        }
        self.state_floor = saved_floor;
        self.operand_stack = saved_operand_stack;
        if pushed_res {
            self.type3_resource_stack.pop();
        }
    }

    fn emit_draw_image(&mut self, image: zpdf_image::DecodedImage) -> Option<zpdf_image::ImageId> {
        let transform = self.current.ctm;
        let image = self.fit_image_to_footprint(image, &transform);
        // Inside standalone `for_type3_glyph()` `d0` replay, an `/ImageMask true`
        // stencil drawn after an explicit fill color operator already had that
        // color baked into its decoded RGBA pixels; clear `is_image_mask` so
        // backends preserve the explicit `d0` color instead of re-tinting.
        let is_image_mask =
            image.is_image_mask && !(self.type3_glyph && self.current.fill_color_explicit);
        let image_id = self.admit_image(image)?;
        self.emit_image_draw(image_id, transform, is_image_mask);
        Some(image_id)
    }

    fn emit_draw_inline_image(
        &mut self,
        image: zpdf_image::DecodedImage,
    ) -> Option<std::sync::Arc<zpdf_display_list::InlineImage>> {
        self.emit_draw_inline_image_at(image, self.current.ctm)
    }

    fn emit_draw_inline_image_at(
        &mut self,
        image: zpdf_image::DecodedImage,
        transform: Matrix,
    ) -> Option<std::sync::Arc<zpdf_display_list::InlineImage>> {
        let image = self.fit_image_to_footprint(image, &transform);
        let bytes = u64::try_from(image.data.capacity()).unwrap_or(u64::MAX);
        if !self.image_room(bytes) {
            self.warn_image_dropped(bytes);
            return None;
        }
        self.type3_inline_fallback_bytes = self.type3_inline_fallback_bytes.saturating_add(bytes);
        let inline = std::sync::Arc::new(zpdf_display_list::InlineImage {
            width: image.width,
            height: image.height,
            data: image.data,
            has_alpha: image.has_alpha,
            is_image_mask: image.is_image_mask,
            premultiplied: image.premultiplied,
        });
        self.emit_inline_image_draw(std::sync::Arc::clone(&inline), transform);
        Some(inline)
    }

    fn admit_image(&mut self, image: zpdf_image::DecodedImage) -> Option<zpdf_image::ImageId> {
        let bytes = u64::try_from(image.data.capacity()).unwrap_or(u64::MAX);
        let id = self.insert_image(image);
        if id.is_none() {
            self.warn_image_dropped(bytes);
        }
        id
    }

    /// Admit an image within the byte limit, which retained sources share.
    fn insert_image(&mut self, image: zpdf_image::DecodedImage) -> Option<zpdf_image::ImageId> {
        let limit = self
            .max_image_cache_bytes
            .saturating_sub(self.retained_image_bytes)
            .saturating_sub(self.type3_inline_fallback_bytes);
        self.image_cache
            .as_mut()?
            .try_insert_with_limit(image, limit)
    }

    fn warn_image_dropped(&mut self, bytes: u64) {
        self.type3_image_dropped = true;
        self.stats.diagnostics.resource_limit_reached = true;
        tracing::warn!(
            "image cache byte limit ({}) or ID capacity reached; dropping {bytes}-byte image",
            self.max_image_cache_bytes
        );
    }

    /// Replay a form XObject. `xobj_id` is the form's object id for a real
    /// stream (used for cached, decryption-aware decoding); `None` marks a
    /// synthetic in-memory stream (e.g. a generated field appearance) whose
    /// bytes are decoded directly — never looked up in the xref.
    fn do_form_xobject(
        &mut self,
        xobj_id: Option<ObjectId>,
        stream: &zpdf_core::PdfStream,
        file: &PdfFile,
    ) {
        const MAX_FORM_DEPTH: u32 = 16;
        if self.form_depth >= MAX_FORM_DEPTH {
            self.nesting_limit_hit = true;
            self.stats.diagnostics.resource_limit_reached = true;
            tracing::warn!("form XObject nesting exceeds {MAX_FORM_DEPTH}; skipping");
            return;
        }
        if let Some(id) = xobj_id.filter(|id| self.active_forms.contains(id)) {
            self.nesting_limit_hit = true;
            self.stats.diagnostics.malformed_content =
                self.stats.diagnostics.malformed_content.saturating_add(1);
            tracing::warn!("form XObject {id} draws itself; skipping the recursion");
            return;
        }
        // Stop descending once the page budget is spent — bounds exponential
        // form fanout (depth is capped but repetition is not).
        if self.over_budget() {
            return;
        }

        // For a real form, resolve_stream_data handles decryption and indirect
        // /Filter refs (with a direct-decode fallback), and the result is kept
        // for the form's later draws. A synthetic stream has no object id, so
        // its bytes are decoded directly — never looked up in the xref /
        // repair machinery.
        let cached = xobj_id.and_then(|id| self.form_content_cache.get(&id).cloned());
        let decoded = match cached {
            Some(decoded) => decoded,
            None => {
                let direct = || decode_stream_for_file(Some(file), &stream.data, &stream.dict);
                let decoded = match xobj_id {
                    Some(id) => file.resolve_stream_data(id).or_else(|_| direct()),
                    None => direct(),
                };
                let decoded: std::sync::Arc<[u8]> = match decoded {
                    Ok(d) => d.into(),
                    Err(e) => {
                        if let Some(id) = xobj_id {
                            self.stats.diagnostics.record_object_failure(id, &e);
                        } else {
                            self.stats.diagnostics.record_decode_failure(&e);
                        }
                        tracing::warn!("failed to decode form XObject: {e}");
                        return;
                    }
                };
                if let Some(id) = xobj_id {
                    if self.reserve_content_cache(decoded.len()) {
                        self.form_content_cache.insert(id, decoded.clone());
                    }
                }
                decoded
            }
        };

        // Apply the form's Matrix if present
        let form_matrix = if let Ok(arr) = stream.dict.get_array("Matrix") {
            let vals: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
            if vals.len() == 6 {
                Matrix::new(vals[0], vals[1], vals[2], vals[3], vals[4], vals[5])
            } else {
                Matrix::identity()
            }
        } else {
            Matrix::identity()
        };

        // Load fonts from the form's own Resources into the FontCache
        self.load_form_fonts(&stream.dict, xobj_id, file);

        // The form's full /Resources (xobjects, gstates, colorspaces,
        // patterns, shadings) shadow the page's for the duration of the form.
        let own_resources = match stream.dict.get("Resources") {
            Some(res_obj) => {
                let resolved = self.resolve_dict_of(res_obj);
                resolved
                    .and_then(|d| zpdf_document::page::parse_resource_dict(&d, file).ok())
                    .map(|res| (res, self.resource_scope_id(res_obj, xobj_id)))
            }
            None => None,
        };
        let form_ctm = self.current.ctm.concat(&form_matrix);
        let pushed_resources = own_resources.is_some();
        if let Some((res, scope_id)) = own_resources {
            self.form_resources
                .push((std::sync::Arc::new(res), form_ctm));
            self.form_resource_scopes.push(scope_id);
        }

        // Save full state including text matrices
        let depth_floor = self.state_stack.len();
        self.state_stack.push(self.current.clone());
        let saved_floor = self.state_floor;
        // The form's own saved state (at depth_floor) is off-limits to the
        // form body's Q operators.
        self.state_floor = depth_floor + 1;
        let saved_text_matrix = self.text_matrix;
        let saved_text_line_matrix = self.text_line_matrix;
        let saved_operand_stack = std::mem::take(&mut self.operand_stack);
        // An unbalanced EMC inside the form must not unsuppress (or leave
        // suppressed) page-level marked-content state. The form's content is a
        // separate marked-content scope, so its /MCIDs start fresh and cannot
        // bleed into (or be polluted by) the page's enclosing sequence.
        let saved_compatibility = std::mem::replace(&mut self.compatibility_depth, 0);
        let saved_mc_depth = self.mc_depth;
        let saved_enclosing_struct_actual_text = self.enclosing_struct_actual_text;
        self.enclosing_struct_actual_text = self.current_struct_actual_text_covers();
        let saved_mc_stack = std::mem::take(&mut self.mc_stack);
        let saved_mc_stream = std::mem::replace(&mut self.current_mc_stream, xobj_id.map(Some));
        let saved_oc_hidden = self.oc_hidden_from;
        let saved_type3_inline_ctx = self.type3_inline_ctx.take();

        if self.form_depth == 0 {
            self.page_base_ctm = self.base_ctm;
        }
        self.current.ctm = form_ctm;
        // Pattern space inside the form anchors to the form's default space
        // (the CTM established by Do + the form /Matrix), and tiling cells
        // defined in this form inherit its entry graphics state (§8.7.3.1).
        let saved_base_ctm = self.base_ctm;
        let saved_base_state = self.base_state.clone();
        let saved_base_font_id = self.base_font_id;
        self.base_ctm = form_ctm;
        self.base_state = self.current.clone();
        self.base_font_id = self.current_font_id;
        self.text_matrix = Matrix::identity();
        self.text_line_matrix = Matrix::identity();
        self.current.clip_depth = 0;

        // /BBox clips the form's content (transformed by the form matrix + CTM).
        let mut bbox_clip = false;
        let mut bbox_bounds: Option<Rect> = None;
        if let Ok(arr) = stream.dict.get_array("BBox") {
            let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
            if v.len() == 4 {
                let (x0, y0) = (v[0].min(v[2]), v[1].min(v[3]));
                let (x1, y1) = (v[0].max(v[2]), v[1].max(v[3]));
                if x1 > x0 && y1 > y0 {
                    let mut bbox_path = Path::new();
                    bbox_path.rect(Rect::new(x0, y0, x1, y1));
                    let page_path = self.transform_path_to_page_space(&bbox_path);
                    bbox_bounds = Self::path_bounds(&page_path);
                    self.intersect_clip_bounds(bbox_bounds);
                    if !self.clip_path_covers_current_bounds(&page_path) {
                        self.intersect_clip_elision_bounds(
                            self.clip_path_enforced_device_bounds(&page_path),
                        );
                        self.display_list.push(RenderCommand::PushClip {
                            path: page_path,
                            rule: FillRule::NonZero,
                        });
                        bbox_clip = true;
                    }
                }
            }
        }

        // /Group /S /Transparency: composite the form as a unit, consuming the
        // current alpha / blend mode / soft mask (PDF 11.6.6 — they reset to
        // defaults inside the group).
        let group = stream
            .dict
            .get("Group")
            .and_then(|o| self.resolve_dict_of(o))
            .filter(|g| matches!(g.get_name("S"), Ok("Transparency")));
        let in_group = group.is_some();
        if let Some(g) = group {
            let flag = |key| matches!(g.get(key), Some(PdfObject::Bool(true)));
            self.display_list.push(RenderCommand::PushBlendGroup {
                blend_mode: self.current.blend_mode,
                isolated: flag("I"),
                knockout: flag("K"),
                bounds: bbox_bounds.unwrap_or(self.display_list.page_rect),
                alpha: self.current.fill_alpha,
                mask: self.current.soft_mask.take(),
            });
            self.current.blend_mode = BlendMode::Normal;
            self.current.fill_alpha = 1.0;
            self.current.stroke_alpha = 1.0;
        }

        self.form_depth += 1;
        self.max_depth_reached = self.max_depth_reached.max(self.form_depth);
        self.active_forms.extend(xobj_id);
        // A pending `W` belongs to the stream that set it: the invoking
        // stream's waits for its own painting operator (a pattern fill runs
        // this form while painting), and one the form leaves unpainted ends
        // with the form.
        let outer_clip = self.pending_clip.take();
        let mut tokenizer = ContentTokenizer::new(&decoded);
        while let Some(token) = tokenizer.next_borrowed_recovering() {
            if self.over_budget() {
                break;
            }
            match token {
                ContentToken::Operand(obj) => {
                    if !self.push_operand(obj) {
                        break;
                    }
                }
                ContentToken::Operator(op) => {
                    self.execute_operator(&op);
                    self.operand_stack.clear();
                }
                ContentToken::InlineImage { dict, data } => {
                    self.do_inline_image(dict, data);
                    self.operand_stack.clear();
                }
            }
        }
        self.stats.diagnostics.merge(tokenizer.diagnostics());
        self.close_marked_content();
        self.pending_clip = outer_clip;
        self.form_depth -= 1;
        if xobj_id.is_some() {
            self.active_forms.pop();
        }

        // Balance: pop clips opened by any unbalanced q-levels left by the
        // form, then restore the state saved at entry.
        while self.state_stack.len() > depth_floor + 1 {
            for _ in 0..self.current.clip_depth {
                self.display_list.push(RenderCommand::PopClip);
            }
            if let Some(state) = self.state_stack.pop() {
                self.current = state;
            }
        }
        for _ in 0..self.current.clip_depth {
            self.display_list.push(RenderCommand::PopClip);
        }
        if let Some(state) = self.state_stack.pop() {
            self.current = state;
        }
        if in_group {
            self.display_list.push(RenderCommand::PopBlendGroup);
        }
        if bbox_clip {
            self.display_list.push(RenderCommand::PopClip);
        }
        self.state_floor = saved_floor;
        self.text_matrix = saved_text_matrix;
        self.text_line_matrix = saved_text_line_matrix;
        self.operand_stack = saved_operand_stack;
        self.mc_depth = saved_mc_depth;
        self.mc_stack = saved_mc_stack;
        self.current_mc_stream = saved_mc_stream;
        self.enclosing_struct_actual_text = saved_enclosing_struct_actual_text;
        self.compatibility_depth = saved_compatibility;
        self.oc_hidden_from = saved_oc_hidden;
        self.type3_inline_ctx = saved_type3_inline_ctx;
        self.base_ctm = saved_base_ctm;
        self.base_state = saved_base_state;
        self.base_font_id = saved_base_font_id;
        self.pop_form_fonts();
        if pushed_resources {
            self.form_resources.pop();
            self.form_resource_scopes.pop();
        }
    }

    /// Make the font dictionary `font_ref` the current font at `size`, as an
    /// ExtGState `/Font` entry does. It is cached under the same object-id key
    /// form fonts use, so the font loads once however often it is selected.
    fn select_font_object(&mut self, font_ref: ObjectId, size: f32) {
        let name = format!("__font_{}_{}", font_ref.0, font_ref.1);
        self.current.font_name = name.clone();
        self.current.font_size = size;
        self.current_font_id = None;
        let (Some(file), Some(fc)) = (self.file, self.font_cache.as_mut()) else {
            return;
        };
        if fc.get_by_name(&name).is_none() {
            let max_bytes = self.max_font_cache_bytes;
            match load_font_ref(self.shared_font_doc, file, fc, font_ref, &name, max_bytes) {
                Ok(Some(diag)) => self.stats.diagnostics.merge(diag),
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!("ExtGState font {font_ref}: {e}");
                    let _ = fc.try_insert_with_limit(
                        name.clone(),
                        zpdf_font::LoadedFont::new_placeholder(name.clone()),
                        max_bytes,
                    );
                }
            }
        }
        self.current_font_id = fc.get_by_name(&name).map(|(fid, _)| fid);
    }

    fn resolve_font_name(&self, name: &str) -> String {
        for overrides in self.form_font_overrides.iter().rev() {
            if let Some(mapped) = overrides.get(name) {
                return mapped.clone();
            }
        }
        name.to_string()
    }

    fn load_form_fonts(
        &mut self,
        form_dict: &zpdf_core::PdfDict,
        form_id: Option<ObjectId>,
        file: &PdfFile,
    ) {
        let (res_dict, owner) = match form_dict.get("Resources") {
            Some(PdfObject::Dict(res)) => {
                (Some(res.clone()), form_id.map(InlineFontOwner::Container))
            }
            Some(PdfObject::Ref(r)) => (
                file.resolve(*r)
                    .ok()
                    .and_then(|o| o.as_dict().ok().cloned()),
                Some(InlineFontOwner::ResourcesDict(*r)),
            ),
            _ => (None, None),
        };
        match res_dict {
            Some(res) => self.load_fonts_from_resources_dict(&res, owner, file),
            None => self.form_font_overrides.push(HashMap::new()),
        }
    }

    fn load_fonts_from_resources_dict(
        &mut self,
        res_dict: &zpdf_core::PdfDict,
        owner: Option<InlineFontOwner>,
        file: &PdfFile,
    ) {
        let bindings = self.prepare_font_bindings(res_dict, owner, file);
        self.push_cached_font_bindings(&bindings);
    }

    /// Resolve and admit fonts from `res_dict`'s `/Font` subdictionary once,
    /// returning lightweight bindings that can be pushed against any enclosing
    /// scope stack via [`Self::push_cached_font_bindings`].
    fn prepare_font_bindings(
        &mut self,
        res_dict: &zpdf_core::PdfDict,
        owner: Option<InlineFontOwner>,
        file: &PdfFile,
    ) -> Vec<CachedFontBinding> {
        let fonts_dict = match res_dict.get("Font") {
            Some(PdfObject::Dict(f)) => Some(f.clone()),
            Some(PdfObject::Ref(r)) => file
                .resolve(*r)
                .ok()
                .and_then(|o| o.as_dict().ok().cloned()),
            _ => None,
        };

        let Some(fonts) = fonts_dict else {
            return Vec::new();
        };

        // Build page font ObjectId mapping for collision detection
        let page_font_ids: HashMap<String, ObjectId> = self
            .resources
            .map(|r| r.fonts.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();

        let mut bindings = Vec::new();
        let max_font_bytes = self.max_font_cache_bytes;
        let shared_doc = self.shared_font_doc;

        let Some(fc) = self.font_cache.as_mut() else {
            return Vec::new();
        };

        // Page resources are capped by MAX_RESOURCE_ENTRIES; a form's raw
        // /Font dictionary bypasses that parser, so bound it here too.
        // Optional embedded companions are stored as lightweight reload
        // references (`Object` / `FormInlineFont`) plus a budget-charged,
        // reclaimable standby copy when it fits, so unused companions never
        // exceed `max_font_bytes` or displace sibling fonts.
        const MAX_FORM_FONT_ENTRIES: usize = 65_536;
        for (name, obj) in fonts.0.iter().take(MAX_FORM_FONT_ENTRIES) {
            match obj {
                PdfObject::Ref(font_ref) => {
                    // If the page already caches this exact font under this same
                    // name, reuse the page's cache slot and record a `PageSlot`
                    // binding so `push_cached_font_bindings` emits an explicit
                    // rebinding whenever an enclosing scope shadows `name`.
                    let page_same_obj = page_font_ids
                        .get(&name.0)
                        .map(|id| *id == *font_ref)
                        .unwrap_or(false);
                    if page_same_obj {
                        bindings.push(CachedFontBinding::PageSlot(name.0.clone()));
                        continue;
                    }

                    // Otherwise scope the cache slot by the font's object id.
                    // Sibling form XObjects routinely reuse the same resource
                    // name for *different* fonts (ArchiCAD/GraphiSoft emit /R7,
                    // /R9 in every form). Keying the shared cache by name — even
                    // by (form-depth, name) — collides for two siblings at the
                    // same depth: the first-loaded font won and every later
                    // form's text rendered with the wrong subset. An object-id
                    // key also dedupes a font shared across forms (loaded once,
                    // reused).
                    let unique_name = format!("__font_{}_{}", font_ref.0, font_ref.1);
                    if fc.get_by_name(&unique_name).is_none() {
                        match load_font_ref(
                            shared_doc,
                            file,
                            fc,
                            *font_ref,
                            &unique_name,
                            max_font_bytes,
                        ) {
                            Ok(Some(diag)) => self.stats.diagnostics.merge(diag),
                            Ok(None) => {}
                            Err(e) => {
                                tracing::debug!("form font {}: {e}", name.0);
                                let _ = fc.try_insert_with_limit(
                                    unique_name.clone(),
                                    zpdf_font::LoadedFont::new_placeholder(name.0.clone()),
                                    max_font_bytes,
                                );
                            }
                        }
                    }
                    bindings.push(CachedFontBinding::CacheSlot(name.0.clone(), unique_name));
                }
                PdfObject::Dict(font_dict) => {
                    // Inline font dict: key the slot by the dictionary itself.
                    // Sibling forms and tiling patterns reuse resource names
                    // for different inline dicts (a name or depth key let the
                    // first one win); a content key tells them apart, yet stays
                    // stable across repeated paints and across interpreters
                    // sharing one FontCache.
                    let target = format!("__inline_font_{:016x}", inline_font_key(font_dict));
                    bindings.push(CachedFontBinding::CacheSlot(name.0.clone(), target.clone()));
                    if let Some((fid, _)) = fc.get_by_name(&target) {
                        if matches!(
                            fc.type3_source(fid),
                            Some(zpdf_font::Type3Source::Unavailable)
                        ) {
                            let upgraded = match owner {
                                Some(InlineFontOwner::Container(owner_id)) => {
                                    fc.record_type3_container_source(fid, owner_id, name.0.clone());
                                    true
                                }
                                Some(InlineFontOwner::ResourcesDict(res_id)) => {
                                    fc.record_type3_resources_source(fid, res_id, name.0.clone());
                                    true
                                }
                                None => false,
                            };
                            if upgraded {
                                self.type3_font_resources.remove(&fid);
                            }
                        }
                    } else {
                        match zpdf_document::font_loader::load_single_font_dict_with_fallback(
                            file,
                            font_dict,
                            fc.provider().map(|provider| provider.as_ref()),
                        ) {
                            Ok((font, diagnostics, companion, descriptor_spec)) => {
                                match fc.admit_with_diagnostics(
                                    target.clone(),
                                    std::sync::Arc::new(font),
                                    diagnostics,
                                    None,
                                    max_font_bytes,
                                ) {
                                    Ok(fid) => {
                                        if fc.get(fid).is_some_and(|f| f.has_type3_programs()) {
                                            match owner {
                                                Some(InlineFontOwner::Container(owner_id)) => {
                                                    fc.record_type3_container_source(
                                                        fid,
                                                        owner_id,
                                                        name.0.clone(),
                                                    );
                                                }
                                                Some(InlineFontOwner::ResourcesDict(res_id)) => {
                                                    fc.record_type3_resources_source(
                                                        fid,
                                                        res_id,
                                                        name.0.clone(),
                                                    );
                                                }
                                                None => {
                                                    if !fc.record_type3_inline_source_with_limit(
                                                        fid,
                                                        std::sync::Arc::new(font_dict.clone()),
                                                        max_font_bytes,
                                                    ) && (font_dict.get("Resources").is_some()
                                                        || font_dict.get("CharProcs").is_some())
                                                    {
                                                        fc.record_type3_source_unavailable(fid);
                                                        self.stats
                                                            .diagnostics
                                                            .resource_limit_reached = true;
                                                    }
                                                }
                                            }
                                        }
                                        if fc.get(fid).is_some_and(|f| f.has_font_data()) {
                                            if let Some(spec) = descriptor_spec {
                                                fc.record_descriptor_substitute(
                                                    fid,
                                                    spec,
                                                    max_font_bytes,
                                                );
                                            } else if companion.is_some() {
                                                let source = match owner {
                                                    Some(InlineFontOwner::Container(id)) => {
                                                        zpdf_font::CompanionSource::FormInlineFont {
                                                            form_id: id,
                                                            font_name: name.0.clone(),
                                                        }
                                                    }
                                                    Some(InlineFontOwner::ResourcesDict(
                                                        resources_id,
                                                    )) => {
                                                        zpdf_font::CompanionSource::ResourcesInlineFont {
                                                            resources_id,
                                                            font_name: name.0.clone(),
                                                        }
                                                    }
                                                    None => zpdf_font::CompanionSource::None,
                                                };
                                                let _ = fc.record_companion(
                                                    fid,
                                                    zpdf_font::CompanionKind::EmbeddedOriginal,
                                                    source,
                                                    companion.map(std::sync::Arc::new),
                                                    max_font_bytes,
                                                );
                                            }
                                        }
                                    }
                                    Err(diag) => self.stats.diagnostics.merge(diag),
                                }
                            }
                            Err(e) => {
                                tracing::debug!("form inline font {}: {e}", name.0);
                                let _ = fc.try_insert_with_limit(
                                    target,
                                    zpdf_font::LoadedFont::new_placeholder(name.0.clone()),
                                    max_font_bytes,
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        bindings
    }

    /// Push a font override scope from cached bindings, resolving `PageSlot`
    /// entries against the currently active parent override stack.
    fn push_cached_font_bindings(&mut self, bindings: &[CachedFontBinding]) {
        let mut overrides = HashMap::new();
        for binding in bindings {
            match binding {
                CachedFontBinding::PageSlot(name) => {
                    if self
                        .form_font_overrides
                        .iter()
                        .any(|m| m.contains_key(name))
                    {
                        overrides.insert(name.clone(), name.clone());
                    }
                }
                CachedFontBinding::CacheSlot(name, target) => {
                    overrides.insert(name.clone(), target.clone());
                }
            }
        }
        if !overrides.is_empty() {
            self.font_repair.scope_changed();
        }
        self.form_font_overrides.push(overrides);
    }

    /// Install the clip a `W`/`W*` left pending (see `pending_clip`).
    fn flush_pending_clip(&mut self) {
        if let Some((path, rule)) = self.pending_clip.take() {
            self.intersect_clip_bounds(Self::path_bounds(&path));
            if self.clip_path_covers_current_bounds(&path) {
                return;
            }
            self.intersect_clip_elision_bounds(self.clip_path_enforced_device_bounds(&path));
            self.display_list
                .push(RenderCommand::PushClip { path, rule });
            self.current.clip_depth += 1;
        }
    }

    fn transform_path_to_page_space(&self, path: &Path) -> Path {
        let ctm = &self.current.ctm;
        if *ctm == Matrix::identity() {
            return path.clone();
        }
        path.map_points(|p| p.transform(ctm))
    }

    fn current_point(&self) -> Point {
        self.current_path.last_point().unwrap_or_else(Point::zero)
    }

    fn ctm_scale_factor(&self) -> f32 {
        let ctm = &self.current.ctm;
        ((ctm.a * ctm.a + ctm.b * ctm.b).sqrt() as f32
            + (ctm.c * ctm.c + ctm.d * ctm.d).sqrt() as f32)
            / 2.0
    }

    fn stroke_style(&self) -> StrokeStyle {
        let scale = self.ctm_scale_factor();
        StrokeStyle {
            width: self.current.line_width * scale,
            cap: self.current.line_cap,
            join: self.current.line_join,
            miter_limit: self.current.miter_limit,
            dash: self.current.dash.as_ref().map(|d| DashPattern {
                array: d.array.iter().map(|v| v * scale).collect(),
                phase: d.phase * scale,
            }),
        }
    }

    /// Max page-space thickness (stroke width, or a filled rect's short side)
    /// for a mark to count as a ruled line, and max rules captured per page.
    const RULE_MAX_THICKNESS: f64 = 3.0;
    const RULE_MAX_COUNT: usize = 20_000;
    /// Min length for a captured rule — shorter marks are dashes/ticks/glyph
    /// underlines, not table grid lines.
    const RULE_MIN_LEN: f64 = 8.0;

    /// True when rule capture is active for this drawing op: a sink is
    /// installed, capacity remains, the mark is visible (not an OC-hidden
    /// layer), and we are on the page proper (not replicating tiling cells,
    /// whose ruled art would flood the sink with per-tile duplicates).
    fn rules_active(&self) -> bool {
        match &self.rule_sink {
            Some(sink) => sink.len() < Self::RULE_MAX_COUNT && !self.oc_suppressed(),
            None => false,
        }
    }

    /// Capture axis-aligned segments of a stroked page-space path as ruled
    /// lines, when the stroke is thin enough to be a rule.
    fn capture_stroke_rules(&mut self, page_path: &Path) {
        if !self.rules_active() {
            return;
        }
        let width = (self.current.line_width * self.ctm_scale_factor()) as f64;
        if !(0.0..=Self::RULE_MAX_THICKNESS).contains(&width) {
            return;
        }
        let mut rules: Vec<RuleLine> = Vec::new();
        let mut cur: Option<Point> = None;
        let mut start: Option<Point> = None;
        for elem in page_path {
            match elem {
                PathElement::MoveTo(p) => {
                    cur = Some(p);
                    start = Some(p);
                }
                PathElement::LineTo(p) => {
                    if let Some(from) = cur {
                        Self::push_segment_rule(&mut rules, from, p);
                    }
                    cur = Some(p);
                }
                PathElement::CurveTo(_, _, p) => cur = Some(p), // curves are not rules
                PathElement::Close => {
                    if let (Some(from), Some(to)) = (cur, start) {
                        Self::push_segment_rule(&mut rules, from, to);
                    }
                    cur = start;
                }
            }
        }
        if let Some(sink) = self.rule_sink.as_mut() {
            let room = Self::RULE_MAX_COUNT.saturating_sub(sink.len());
            sink.extend(rules.into_iter().take(room));
        }
    }

    /// Capture a filled page-space path as ruled lines when it consists of
    /// thin axis-aligned rectangles (rules are often drawn with `re f`, not a
    /// stroke). Each 4/5-element rect subpath with a short side within the
    /// thickness cap becomes one rule along its long side's midline.
    fn capture_fill_rules(&mut self, page_path: &Path) {
        if !self.rules_active() {
            return;
        }
        let mut rules: Vec<RuleLine> = Vec::new();
        // Walk subpaths, tracking an axis-aligned bbox and point count; a
        // subpath containing a curve disqualifies itself.
        let (mut n, mut curved) = (0usize, false);
        let (mut bx0, mut by0, mut bx1, mut by1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        let flush = |n: &mut usize,
                     curved: &mut bool,
                     bx0: &mut f64,
                     by0: &mut f64,
                     bx1: &mut f64,
                     by1: &mut f64,
                     rules: &mut Vec<RuleLine>| {
            if *n >= 4 && !*curved && bx1 >= bx0 && by1 >= by0 {
                let w = *bx1 - *bx0;
                let h = *by1 - *by0;
                if h <= Self::RULE_MAX_THICKNESS && w >= Self::RULE_MIN_LEN {
                    rules.push(RuleLine {
                        vertical: false,
                        pos: (*by0 + *by1) / 2.0,
                        start: *bx0,
                        end: *bx1,
                    });
                } else if w <= Self::RULE_MAX_THICKNESS && h >= Self::RULE_MIN_LEN {
                    rules.push(RuleLine {
                        vertical: true,
                        pos: (*bx0 + *bx1) / 2.0,
                        start: *by0,
                        end: *by1,
                    });
                }
            }
            (*n, *curved) = (0, false);
            (*bx0, *by0, *bx1, *by1) = (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            );
        };
        for elem in page_path {
            match elem {
                PathElement::MoveTo(p) => {
                    flush(
                        &mut n,
                        &mut curved,
                        &mut bx0,
                        &mut by0,
                        &mut bx1,
                        &mut by1,
                        &mut rules,
                    );
                    n = 1;
                    (bx0, by0, bx1, by1) = (p.x, p.y, p.x, p.y);
                }
                PathElement::LineTo(p) => {
                    n += 1;
                    bx0 = bx0.min(p.x);
                    by0 = by0.min(p.y);
                    bx1 = bx1.max(p.x);
                    by1 = by1.max(p.y);
                }
                PathElement::CurveTo(..) => curved = true,
                PathElement::Close => {}
            }
        }
        flush(
            &mut n,
            &mut curved,
            &mut bx0,
            &mut by0,
            &mut bx1,
            &mut by1,
            &mut rules,
        );
        if let Some(sink) = self.rule_sink.as_mut() {
            let room = Self::RULE_MAX_COUNT.saturating_sub(sink.len());
            sink.extend(rules.into_iter().take(room));
        }
    }

    /// Append `from → to` as a rule if it is axis-aligned (within a hair) and
    /// long enough to be a grid line rather than a tick.
    fn push_segment_rule(rules: &mut Vec<RuleLine>, from: Point, to: Point) {
        const AXIS_TOL: f64 = 0.5; // page-space units of allowed skew
        let dx = (to.x - from.x).abs();
        let dy = (to.y - from.y).abs();
        if dy <= AXIS_TOL && dx >= Self::RULE_MIN_LEN {
            rules.push(RuleLine {
                vertical: false,
                pos: (from.y + to.y) / 2.0,
                start: from.x.min(to.x),
                end: from.x.max(to.x),
            });
        } else if dx <= AXIS_TOL && dy >= Self::RULE_MIN_LEN {
            rules.push(RuleLine {
                vertical: true,
                pos: (from.x + to.x) / 2.0,
                start: from.y.min(to.y),
                end: from.y.max(to.y),
            });
        }
    }

    /// The current path in page space, ending the path object. The user-space
    /// buffer is cleared rather than dropped, so the next path reuses it.
    fn take_page_path(&mut self) -> Option<Path> {
        if self.current_path.is_empty() {
            return None;
        }
        let page_path = self.transform_path_to_page_space(&self.current_path);
        self.current_path.clear();
        Some(page_path)
    }

    fn paint_stroke(&mut self) {
        if self.current_path.is_empty() {
            return;
        }
        if self.should_expand_anisotropic_stroke() {
            let remaining = self
                .max_anisotropic_stroke_points
                .saturating_sub(self.anisotropic_stroke_points);
            match self.expand_anisotropic_stroke(&self.current_path, remaining) {
                AnisotropicStroke::Outline(outline) => {
                    if self.rules_active() {
                        let page_path = self.transform_path_to_page_space(&self.current_path);
                        self.capture_stroke_rules(&page_path);
                    }
                    self.anisotropic_stroke_points = self
                        .anisotropic_stroke_points
                        .saturating_add(outline.points().len());
                    self.current_path.clear();
                    self.emit_stroke_as_filled_outline(outline);
                    return;
                }
                AnisotropicStroke::Empty => {
                    self.current_path.clear();
                    return;
                }
                AnisotropicStroke::Fallback => {}
            }
        }
        if let Some(page_path) = self.take_page_path() {
            self.emit_stroke(page_path);
        }
    }

    fn paint_fill(&mut self, rule: FillRule) {
        if let Some(page_path) = self.take_page_path() {
            self.fill_page_path(page_path, rule);
        }
    }

    fn paint_fill_then_stroke(&mut self, rule: FillRule) {
        if self.current_path.is_empty() {
            return;
        }
        let page_path = self.transform_path_to_page_space(&self.current_path);
        if self.should_expand_anisotropic_stroke() {
            let remaining = self
                .max_anisotropic_stroke_points
                .saturating_sub(self.anisotropic_stroke_points);
            let expanded = self.expand_anisotropic_stroke(&self.current_path, remaining);
            if let AnisotropicStroke::Outline(ref outline) = expanded {
                self.anisotropic_stroke_points = self
                    .anisotropic_stroke_points
                    .saturating_add(outline.points().len());
            }
            self.current_path.clear();
            self.fill_page_path(page_path.clone(), rule);
            match expanded {
                AnisotropicStroke::Outline(outline) => {
                    self.capture_stroke_rules(&page_path);
                    self.emit_stroke_as_filled_outline(outline);
                }
                AnisotropicStroke::Empty => {}
                AnisotropicStroke::Fallback => self.emit_stroke(page_path),
            }
            return;
        }
        self.current_path.clear();
        self.fill_page_path(page_path.clone(), rule);
        self.emit_stroke(page_path);
    }

    /// True when the active stroke has a positive finite user-space width under
    /// a non-similarity (anisotropic scale or shear) CTM, so scalar width
    /// scaling would distort directional pen widths. Hairlines (`line_width == 0`)
    /// stay as `StrokePath` because their device width is independent of CTM.
    fn should_expand_anisotropic_stroke(&self) -> bool {
        if !self.current.line_width.is_finite() || self.current.line_width <= 0.0 {
            return false;
        }
        let ctm = &self.current.ctm;
        if ![ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f]
            .iter()
            .all(|v| v.is_finite())
        {
            return false;
        }
        let col_x_sq = ctm.a * ctm.a + ctm.b * ctm.b;
        let col_y_sq = ctm.c * ctm.c + ctm.d * ctm.d;
        let ref_sq = col_x_sq.max(col_y_sq);
        if !ref_sq.is_finite() || ref_sq <= 1e-24 {
            return false;
        }
        let dot = ctm.a * ctm.c + ctm.b * ctm.d;
        (col_x_sq - col_y_sq).abs() > 1e-4 * ref_sq || dot.abs() > 1e-4 * ref_sq
    }

    fn largest_singular_value(m: &Matrix) -> f64 {
        let sum_sq = m.a * m.a + m.b * m.b + m.c * m.c + m.d * m.d;
        let det = m.a * m.d - m.b * m.c;
        let disc = (sum_sq * sum_sq - 4.0 * det * det).max(0.0).sqrt();
        (0.5 * (sum_sq + disc)).sqrt()
    }

    fn split_cubic_half(p0: Point, c1: Point, c2: Point, p3: Point) -> ([Point; 4], [Point; 4]) {
        let mid = |a: Point, b: Point| Point::new(0.5 * (a.x + b.x), 0.5 * (a.y + b.y));
        let m01 = mid(p0, c1);
        let m12 = mid(c1, c2);
        let m23 = mid(c2, p3);
        let m012 = mid(m01, m12);
        let m123 = mid(m12, m23);
        let m0123 = mid(m012, m123);
        ([p0, m01, m012, m0123], [m0123, m123, m23, p3])
    }

    fn cubic_gl5_length(p0: Point, c1: Point, c2: Point, p3: Point) -> f64 {
        const NODES: [f64; 5] = [
            0.046_910_077_030_668,
            0.230_765_344_947_158_45,
            0.5,
            0.769_234_655_052_841_5,
            0.953_089_922_969_332,
        ];
        const WEIGHTS: [f64; 5] = [
            0.118_463_442_528_094_54,
            0.239_314_335_249_683_24,
            0.284_444_444_444_444_44,
            0.239_314_335_249_683_24,
            0.118_463_442_528_094_54,
        ];
        let ax = c1.x - p0.x;
        let ay = c1.y - p0.y;
        let bx = c2.x - c1.x;
        let by = c2.y - c1.y;
        let cx = p3.x - c2.x;
        let cy = p3.y - c2.y;
        let mut sum = 0.0;
        for (&t, &w) in NODES.iter().zip(WEIGHTS.iter()) {
            let u = 1.0 - t;
            let dx = 3.0 * (u * u * ax + 2.0 * u * t * bx + t * t * cx);
            let dy = 3.0 * (u * u * ay + 2.0 * u * t * by + t * t * cy);
            sum += w * dx.hypot(dy);
        }
        sum
    }

    fn cubic_arc_length(p0: Point, c1: Point, c2: Point, p3: Point, tol: f64) -> Option<f64> {
        let poly = (c1.x - p0.x).hypot(c1.y - p0.y)
            + (c2.x - c1.x).hypot(c2.y - c1.y)
            + (p3.x - c2.x).hypot(p3.y - c2.y);
        if !poly.is_finite() {
            return None;
        }
        if poly == 0.0 {
            return Some(0.0);
        }
        let mut stack: Vec<(Point, Point, Point, Point, f64, u8)> =
            vec![(p0, c1, c2, p3, Self::cubic_gl5_length(p0, c1, c2, p3), 0)];
        let mut total = 0.0f64;
        let mut splits_left = 1024usize;
        while let Some((a, b, c, d, whole, depth)) = stack.pop() {
            if !whole.is_finite() {
                return None;
            }
            if depth >= 12 || splits_left == 0 {
                total += whole;
                continue;
            }
            let (left, right) = Self::split_cubic_half(a, b, c, d);
            let l_len = Self::cubic_gl5_length(left[0], left[1], left[2], left[3]);
            let r_len = Self::cubic_gl5_length(right[0], right[1], right[2], right[3]);
            let approx = l_len + r_len;
            if !approx.is_finite() {
                return None;
            }
            if (approx - whole).abs() <= tol.max(approx * 1e-9) {
                total += approx;
            } else {
                splits_left -= 1;
                stack.push((right[0], right[1], right[2], right[3], r_len, depth + 1));
                stack.push((left[0], left[1], left[2], left[3], l_len, depth + 1));
            }
        }
        total.is_finite().then_some(total)
    }

    fn push_skia_segment(pb: &mut tiny_skia_path::PathBuilder, seg: tiny_skia_path::PathSegment) {
        match seg {
            tiny_skia_path::PathSegment::MoveTo(p) => pb.move_to(p.x, p.y),
            tiny_skia_path::PathSegment::LineTo(p) => pb.line_to(p.x, p.y),
            tiny_skia_path::PathSegment::QuadTo(p1, p2) => pb.quad_to(p1.x, p1.y, p2.x, p2.y),
            tiny_skia_path::PathSegment::CubicTo(p1, p2, p3) => {
                pb.cubic_to(p1.x, p1.y, p2.x, p2.y, p3.x, p3.y)
            }
            tiny_skia_path::PathSegment::Close => pb.close(),
        }
    }

    /// When a partially clipped closed contour has both its trailing dashed
    /// contour (`dashed_last`) ending at the closing corner `(0, 0)` and its
    /// initial dashed contour (`dashed_first`) starting at `(0, 0)`, merge
    /// those two contours into one continuous contour passing through `(0, 0)`
    /// so stroking produces a proper join instead of two end caps.
    fn join_dashed_closing_contours(
        dashed_last: &tiny_skia_path::Path,
        dashed_first: &tiny_skia_path::Path,
    ) -> Option<tiny_skia_path::Path> {
        let split_contours = |path: &tiny_skia_path::Path| {
            let mut contours: Vec<Vec<tiny_skia_path::PathSegment>> = Vec::new();
            for seg in path.segments() {
                match seg {
                    tiny_skia_path::PathSegment::MoveTo(_) => contours.push(vec![seg]),
                    _ => {
                        if let Some(cur) = contours.last_mut() {
                            cur.push(seg);
                        }
                    }
                }
            }
            contours
        };
        let last_contours = split_contours(dashed_last);
        let first_contours = split_contours(dashed_first);
        let last_c = last_contours.last()?;
        let first_c = first_contours.first()?;
        let (ex, ey) = match last_c.last()? {
            tiny_skia_path::PathSegment::LineTo(p) => (p.x, p.y),
            tiny_skia_path::PathSegment::QuadTo(_, p) => (p.x, p.y),
            tiny_skia_path::PathSegment::CubicTo(_, _, p) => (p.x, p.y),
            tiny_skia_path::PathSegment::MoveTo(_) | tiny_skia_path::PathSegment::Close => {
                return None;
            }
        };
        let (sx, sy) = match first_c.first()? {
            tiny_skia_path::PathSegment::MoveTo(p) if first_c.len() >= 2 => (p.x, p.y),
            _ => return None,
        };
        if matches!(first_c.last(), Some(tiny_skia_path::PathSegment::Close)) {
            return None;
        }
        if ex.hypot(ey) > 1e-2 || sx.hypot(sy) > 1e-2 {
            return None;
        }

        let mut pb = tiny_skia_path::PathBuilder::new();
        for c in &last_contours[..last_contours.len() - 1] {
            for &seg in c {
                Self::push_skia_segment(&mut pb, seg);
            }
        }
        for &seg in last_c {
            Self::push_skia_segment(&mut pb, seg);
        }
        for &seg in &first_c[1..] {
            Self::push_skia_segment(&mut pb, seg);
        }
        for c in &first_contours[1..] {
            for &seg in c {
                Self::push_skia_segment(&mut pb, seg);
            }
        }
        pb.finish()
    }

    /// Stroke `user_path` in isotropic user space (normalized to final
    /// page-point magnitude by the largest singular value of
    /// `shading_space_to_page * ctm`) and transform the resulting outline into
    /// current display-list space (page space, or cell space inside a tiling
    /// pattern). Preserves exact directional widths, elliptical caps/joins, and
    /// transformed dash lengths under nonuniform scales and shears, including
    /// fine dashes inside magnified pattern tiles and mixed compound/curved
    /// paths with off-page geometry.
    fn expand_anisotropic_stroke(
        &self,
        user_path: &Path,
        remaining_points: usize,
    ) -> AnisotropicStroke {
        const RES_SCALE: f32 = 4.0;
        const MAX_NORM_COORD: f32 = 1e7;
        const MAX_DASH_REPEATS: f64 = 100_000.0;

        if remaining_points == 0 {
            return AnisotropicStroke::Fallback;
        }

        #[derive(Clone, Copy)]
        enum UserPathSeg {
            Line(Point),
            Cubic(Point, Point, Point),
        }
        struct UserSubpath {
            start: Point,
            segments: Vec<UserPathSeg>,
            closed: bool,
        }
        struct ClippedRun {
            start: Point,
            segments: Vec<UserPathSeg>,
            start_phase_user: f64,
            starts_at_subpath_start: bool,
            ends_at_subpath_start: bool,
            closed: bool,
        }
        enum VisUserBounds {
            Uncomputed,
            Empty,
            Singular,
            Rect(Rect),
        }

        let ctm = &self.current.ctm;
        let ctm_max_scale = Self::largest_singular_value(ctm);
        if !ctm_max_scale.is_finite() || ctm_max_scale <= 1e-12 {
            return AnisotropicStroke::Fallback;
        }
        let user_to_page = self.shading_space_to_page.concat(ctm);
        let page_max_scale = Self::largest_singular_value(&user_to_page);
        let dl_to_page_scale = Self::largest_singular_value(&self.shading_space_to_page);
        let mut norm_scale = if page_max_scale.is_finite() && page_max_scale > 1e-12 {
            page_max_scale
        } else {
            ctm_max_scale
        };

        // Split into independent PDF subpaths (ISO 32000-1 §8.4.3.6: each
        // subpath is stroked and dashed independently from its own start).
        let mut subpaths: Vec<UserSubpath> = Vec::new();
        let mut cur_sp: Option<UserSubpath> = None;
        let mut after_close_start: Option<Point> = None;
        for elem in user_path {
            match elem {
                PathElement::MoveTo(p) => {
                    if !p.x.is_finite() || !p.y.is_finite() {
                        return AnisotropicStroke::Fallback;
                    }
                    if let Some(sp) = cur_sp.take() {
                        if !sp.segments.is_empty() || sp.closed {
                            subpaths.push(sp);
                        }
                    }
                    cur_sp = Some(UserSubpath {
                        start: p,
                        segments: Vec::new(),
                        closed: false,
                    });
                    after_close_start = None;
                }
                PathElement::LineTo(p) => {
                    if !p.x.is_finite() || !p.y.is_finite() {
                        return AnisotropicStroke::Fallback;
                    }
                    if cur_sp.is_none() {
                        if let Some(s) = after_close_start.take() {
                            cur_sp = Some(UserSubpath {
                                start: s,
                                segments: Vec::new(),
                                closed: false,
                            });
                        }
                    }
                    if let Some(ref mut sp) = cur_sp {
                        sp.segments.push(UserPathSeg::Line(p));
                    }
                }
                PathElement::CurveTo(c1, c2, end) => {
                    if ![c1.x, c1.y, c2.x, c2.y, end.x, end.y]
                        .iter()
                        .all(|v| v.is_finite())
                    {
                        return AnisotropicStroke::Fallback;
                    }
                    if cur_sp.is_none() {
                        if let Some(s) = after_close_start.take() {
                            cur_sp = Some(UserSubpath {
                                start: s,
                                segments: Vec::new(),
                                closed: false,
                            });
                        }
                    }
                    if let Some(ref mut sp) = cur_sp {
                        sp.segments.push(UserPathSeg::Cubic(c1, c2, end));
                    }
                }
                PathElement::Close => {
                    if let Some(mut sp) = cur_sp.take() {
                        sp.closed = true;
                        after_close_start = Some(sp.start);
                        subpaths.push(sp);
                    }
                }
            }
        }
        if let Some(sp) = cur_sp.take() {
            if !sp.segments.is_empty() || sp.closed {
                subpaths.push(sp);
            }
        }
        if subpaths.is_empty() {
            return AnisotropicStroke::Empty;
        }

        let dash_user = self
            .current
            .dash
            .as_ref()
            .filter(|d| {
                !d.array.is_empty()
                    && d.phase.is_finite()
                    && d.array.iter().all(|v| v.is_finite() && *v >= 0.0)
                    && d.array.iter().any(|&v| v > 0.0)
            })
            .map(|d| {
                let mut user_array: Vec<f64> = d.array.iter().map(|&v| v as f64).collect();
                if user_array.len() % 2 == 1 {
                    let len = user_array.len();
                    user_array.reserve(len);
                    for i in 0..len {
                        user_array.push(user_array[i]);
                    }
                }
                let period_user: f64 = user_array.iter().sum();
                let phase_user = if period_user.is_finite() && period_user > 0.0 {
                    (d.phase as f64).rem_euclid(period_user)
                } else {
                    0.0
                };
                (user_array, period_user, phase_user)
            });

        // If a positive dash pattern is still sub-millipoint even after
        // accounting for the final page scale, lift `norm_scale` so tiny-skia
        // preserves the gaps instead of collapsing them to a solid stroke.
        if let Some((_, period_user, _)) = dash_user {
            if period_user.is_finite() && period_user > 0.0 && period_user * norm_scale < 1e-2 {
                let max_for_width =
                    (MAX_NORM_COORD as f64) / (self.current.line_width as f64).max(1e-12);
                let boosted = (1.0 / period_user).min(max_for_width);
                if boosted.is_finite() && boosted > norm_scale {
                    norm_scale = boosted;
                }
            }
        }

        let width_norm = (self.current.line_width as f64 * norm_scale) as f32;
        if !width_norm.is_finite() || width_norm <= 0.0 || width_norm > MAX_NORM_COORD {
            return AnisotropicStroke::Fallback;
        }

        let dash_spec = match dash_user {
            Some((user_array, period_user, phase_user)) => {
                if !period_user.is_finite() || period_user <= 0.0 {
                    return AnisotropicStroke::Fallback;
                }
                let norm_array: Vec<f32> = user_array
                    .iter()
                    .map(|&v| (v * norm_scale) as f32)
                    .collect();
                let period_norm: f64 = norm_array.iter().map(|&v| v as f64).sum();
                if !period_norm.is_finite()
                    || period_norm <= 1e-4
                    || !norm_array.iter().all(|v| v.is_finite())
                {
                    // Never silently drop a positive dash pattern into a solid
                    // stroke; preserve the dash array on deferred StrokePath.
                    return AnisotropicStroke::Fallback;
                }
                Some((norm_array, period_norm, period_user, phase_user))
            }
            None => None,
        };

        let skia_stroke = tiny_skia_path::Stroke {
            width: width_norm,
            miter_limit: if self.current.miter_limit.is_finite() && self.current.miter_limit >= 1.0
            {
                self.current.miter_limit
            } else {
                1.0
            },
            line_cap: match self.current.line_cap {
                LineCap::Butt => tiny_skia_path::LineCap::Butt,
                LineCap::Round => tiny_skia_path::LineCap::Round,
                LineCap::Square => tiny_skia_path::LineCap::Square,
            },
            line_join: match self.current.line_join {
                LineJoin::Miter => tiny_skia_path::LineJoin::Miter,
                LineJoin::Round => tiny_skia_path::LineJoin::Round,
                LineJoin::Bevel => tiny_skia_path::LineJoin::Bevel,
            },
            dash: None,
        };

        let inv_norm = 1.0 / norm_scale;
        if !inv_norm.is_finite() {
            return AnisotropicStroke::Fallback;
        }
        let make_ctm_post = |origin: Point| -> Matrix {
            Matrix::new(
                ctm.a * inv_norm,
                ctm.b * inv_norm,
                ctm.c * inv_norm,
                ctm.d * inv_norm,
                ctm.a * origin.x + ctm.c * origin.y + ctm.e,
                ctm.b * origin.x + ctm.d * origin.y + ctm.f,
            )
        };
        let to_norm = |p: Point, origin: Point| -> Option<(f32, f32)> {
            let nx = ((p.x - origin.x) * norm_scale) as f32;
            let ny = ((p.y - origin.y) * norm_scale) as f32;
            if !nx.is_finite()
                || !ny.is_finite()
                || nx.abs() > MAX_NORM_COORD
                || ny.abs() > MAX_NORM_COORD
            {
                return None;
            }
            Some((nx, ny))
        };
        let build_skia_path = |origin: Point,
                               start: Point,
                               segs: &[UserPathSeg],
                               closed: bool|
         -> Result<Option<tiny_skia_path::Path>, ()> {
            let Some((sx, sy)) = to_norm(start, origin) else {
                return Err(());
            };
            let mut pb = tiny_skia_path::PathBuilder::new();
            pb.move_to(sx, sy);
            for &seg in segs {
                match seg {
                    UserPathSeg::Line(p) => {
                        let Some((nx, ny)) = to_norm(p, origin) else {
                            return Err(());
                        };
                        pb.line_to(nx, ny);
                    }
                    UserPathSeg::Cubic(c1, c2, end) => {
                        let (Some((x1, y1)), Some((x2, y2)), Some((xe, ye))) = (
                            to_norm(c1, origin),
                            to_norm(c2, origin),
                            to_norm(end, origin),
                        ) else {
                            return Err(());
                        };
                        pb.cubic_to(x1, y1, x2, y2, xe, ye);
                    }
                }
            }
            if closed {
                pb.close();
            }
            Ok(pb.finish())
        };
        let make_skia_dash = |phase_user: f64| -> Option<tiny_skia_path::StrokeDash> {
            let (ref norm_array, period_norm, _, _) = dash_spec.as_ref()?;
            let mut phase = (phase_user * norm_scale).rem_euclid(*period_norm) as f32;
            if !phase.is_finite() || phase >= *period_norm as f32 {
                phase = 0.0;
            }
            tiny_skia_path::StrokeDash::new(norm_array.clone(), phase)
        };
        let stroke_into = |path: &tiny_skia_path::Path, origin: Point, out: &mut Path| -> bool {
            let Some(stroked) = path.stroke(&skia_stroke, RES_SCALE) else {
                return true;
            };
            let ctm_post = make_ctm_post(origin);
            Self::append_transformed_skia_path(&stroked, &ctm_post, out, remaining_points)
        };
        let intervals_per_period = dash_spec
            .as_ref()
            .map_or(1usize, |(arr, _, _, _)| (arr.len() / 2).max(1));
        let min_points_for_dashes = |dashes: f64| -> usize {
            if !dashes.is_finite() || dashes <= 0.0 {
                return 0;
            }
            let whole = dashes.floor();
            if whole >= (usize::MAX / 8) as f64 {
                return usize::MAX;
            }
            (whole as usize)
                .saturating_mul(4)
                .saturating_mul(intervals_per_period)
        };

        let mut vis_user_cache = VisUserBounds::Uncomputed;
        let compute_vis_user = |cache: &mut VisUserBounds| -> Option<Rect> {
            if matches!(cache, VisUserBounds::Uncomputed) {
                *cache = 'compute: {
                    let Some(inv_ctm) = ctm.inverse() else {
                        break 'compute VisUserBounds::Singular;
                    };
                    let mut vis_dl = self.display_list.page_rect.normalize();
                    if let Some(clip) = self.current.clip_bounds {
                        let c = clip.normalize();
                        let ix0 = vis_dl.x0.max(c.x0);
                        let iy0 = vis_dl.y0.max(c.y0);
                        let ix1 = vis_dl.x1.min(c.x1);
                        let iy1 = vis_dl.y1.min(c.y1);
                        if ix1 <= ix0 || iy1 <= iy0 {
                            break 'compute VisUserBounds::Empty;
                        }
                        vis_dl = Rect::new(ix0, iy0, ix1, iy1);
                    }
                    let reach = match self.current.line_join {
                        LineJoin::Miter => self.current.miter_limit.clamp(1.0, 100.0) as f64,
                        _ => std::f64::consts::SQRT_2,
                    };
                    let width_dl = self.current.line_width as f64 * ctm_max_scale;
                    let margin_dl = if dl_to_page_scale.is_finite() && dl_to_page_scale > 1e-12 {
                        2.0 / dl_to_page_scale
                    } else {
                        2.0
                    };
                    let pad = width_dl * 0.5 * reach.max(std::f64::consts::SQRT_2) + margin_dl;
                    let padded = Rect::new(
                        vis_dl.x0 - pad,
                        vis_dl.y0 - pad,
                        vis_dl.x1 + pad,
                        vis_dl.y1 + pad,
                    );
                    let corners = [
                        Point::new(padded.x0, padded.y0).transform(&inv_ctm),
                        Point::new(padded.x1, padded.y0).transform(&inv_ctm),
                        Point::new(padded.x0, padded.y1).transform(&inv_ctm),
                        Point::new(padded.x1, padded.y1).transform(&inv_ctm),
                    ];
                    let (mut ux0, mut uy0) = (f64::INFINITY, f64::INFINITY);
                    let (mut ux1, mut uy1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                    for c in &corners {
                        if !c.x.is_finite() || !c.y.is_finite() {
                            break 'compute VisUserBounds::Singular;
                        }
                        ux0 = ux0.min(c.x);
                        uy0 = uy0.min(c.y);
                        ux1 = ux1.max(c.x);
                        uy1 = uy1.max(c.y);
                    }
                    VisUserBounds::Rect(Rect::new(ux0, uy0, ux1, uy1))
                };
            }
            match *cache {
                VisUserBounds::Rect(r) => Some(r),
                _ => None,
            }
        };

        let mut out = Path::new();
        let mut cumulative_dashes = 0.0f64;

        for sp in &subpaths {
            // Fast path: try building this subpath directly in its own local
            // normalized coordinates without clipping.
            let mut pb = tiny_skia_path::PathBuilder::new();
            pb.move_to(0.0, 0.0);
            let mut cur = sp.start;
            let mut upper_len_user = 0.0f64;
            let mut exceeded_norm = false;
            for &seg in &sp.segments {
                match seg {
                    UserPathSeg::Line(p) => {
                        let Some((nx, ny)) = to_norm(p, sp.start) else {
                            exceeded_norm = true;
                            break;
                        };
                        upper_len_user += (p.x - cur.x).hypot(p.y - cur.y);
                        pb.line_to(nx, ny);
                        cur = p;
                    }
                    UserPathSeg::Cubic(c1, c2, end) => {
                        let (Some((x1, y1)), Some((x2, y2)), Some((xe, ye))) = (
                            to_norm(c1, sp.start),
                            to_norm(c2, sp.start),
                            to_norm(end, sp.start),
                        ) else {
                            exceeded_norm = true;
                            break;
                        };
                        upper_len_user += (c1.x - cur.x).hypot(c1.y - cur.y)
                            + (c2.x - c1.x).hypot(c2.y - c1.y)
                            + (end.x - c2.x).hypot(end.y - c2.y);
                        pb.cubic_to(x1, y1, x2, y2, xe, ye);
                        cur = end;
                    }
                }
            }
            if !exceeded_norm && sp.closed {
                upper_len_user += (sp.start.x - cur.x).hypot(sp.start.y - cur.y);
                pb.close();
            }

            let sp_dashes = dash_spec
                .as_ref()
                .map(|&(_, _, period_user, _)| upper_len_user / period_user)
                .unwrap_or(0.0);
            let fits_fast_path = !exceeded_norm
                && upper_len_user.is_finite()
                && (dash_spec.is_none()
                    || (sp_dashes.is_finite()
                        && cumulative_dashes + sp_dashes <= MAX_DASH_REPEATS));
            let try_clip = !fits_fast_path || (dash_spec.is_some() && sp_dashes > 1024.0);

            let vis_user_for_clip = if try_clip {
                match compute_vis_user(&mut vis_user_cache) {
                    Some(r) => Some(r),
                    None if matches!(vis_user_cache, VisUserBounds::Empty) => continue,
                    None if fits_fast_path => None,
                    None => return AnisotropicStroke::Fallback,
                }
            } else {
                None
            };

            let Some(vis_user) = vis_user_for_clip else {
                cumulative_dashes += sp_dashes;
                let min_needed = if dash_spec.is_some() {
                    min_points_for_dashes(sp_dashes)
                } else {
                    sp.segments.len().saturating_mul(2)
                };
                if out.points().len().saturating_add(min_needed) > remaining_points {
                    return AnisotropicStroke::Fallback;
                }
                let Some(norm_path) = pb.finish() else {
                    continue;
                };
                if let Some((_, _, _, phase_user)) = dash_spec {
                    let Some(skia_dash) = make_skia_dash(phase_user) else {
                        return AnisotropicStroke::Fallback;
                    };
                    let Some(dashed) = norm_path.dash(&skia_dash, RES_SCALE) else {
                        continue;
                    };
                    if !stroke_into(&dashed, sp.start, &mut out) {
                        return AnisotropicStroke::Fallback;
                    }
                } else if !stroke_into(&norm_path, sp.start, &mut out) {
                    return AnisotropicStroke::Fallback;
                }
                continue;
            };

            // Whole-subpath convex-hull culling: off-page lines or curves
            // disjoint from `vis_user` contribute no visible pixels.
            let (mut bx0, mut by0, mut bx1, mut by1) =
                (sp.start.x, sp.start.y, sp.start.x, sp.start.y);
            for &seg in &sp.segments {
                match seg {
                    UserPathSeg::Line(p) => {
                        bx0 = bx0.min(p.x);
                        by0 = by0.min(p.y);
                        bx1 = bx1.max(p.x);
                        by1 = by1.max(p.y);
                    }
                    UserPathSeg::Cubic(c1, c2, end) => {
                        for p in [c1, c2, end] {
                            bx0 = bx0.min(p.x);
                            by0 = by0.min(p.y);
                            bx1 = bx1.max(p.x);
                            by1 = by1.max(p.y);
                        }
                    }
                }
            }
            if bx1 < vis_user.x0 || bx0 > vis_user.x1 || by1 < vis_user.y0 || by0 > vis_user.y1 {
                continue;
            }

            let period_user = dash_spec.as_ref().map(|&(_, _, pu, _)| pu);
            let initial_phase_user = dash_spec.as_ref().map(|&(_, _, _, ph)| ph).unwrap_or(0.0);
            let phase_tol = period_user.map_or(1.0, |p| (p * 1e-4).max(1e-9));

            struct ClipCtx<'a> {
                vis_user: Rect,
                period_user: Option<f64>,
                phase_tol: f64,
                max_dash_repeats: f64,
                phase: f64,
                cumulative_dashes: &'a mut f64,
                any_clipped: bool,
                at_subpath_start: bool,
                active_run: Option<ClippedRun>,
                visible_runs: Vec<ClippedRun>,
            }
            impl<'a> ClipCtx<'a> {
                fn push_line(&mut self, a: Point, b: Point) -> Result<(), ()> {
                    let seg_len = (b.x - a.x).hypot(b.y - a.y);
                    if !seg_len.is_finite() {
                        return Err(());
                    }
                    if let Some((t0, t1)) =
                        ContentInterpreter::clip_segment_to_rect(a, b, self.vis_user)
                    {
                        if t0 > 0.0 || t1 < 1.0 {
                            self.any_clipped = true;
                        }
                        let p0 = Point::new(a.x + t0 * (b.x - a.x), a.y + t0 * (b.y - a.y));
                        let p1 = Point::new(a.x + t1 * (b.x - a.x), a.y + t1 * (b.y - a.y));
                        if let Some(pu) = self.period_user {
                            let vis_dashes = ((t1 - t0) * seg_len) / pu;
                            *self.cumulative_dashes += vis_dashes;
                            if !self.cumulative_dashes.is_finite()
                                || *self.cumulative_dashes > self.max_dash_repeats
                            {
                                return Err(());
                            }
                        }
                        if t0 > 0.0 || self.active_run.is_none() {
                            if let Some(prev) = self.active_run.take() {
                                self.visible_runs.push(prev);
                            }
                            let start_phase = match self.period_user {
                                Some(pu) => (self.phase + t0 * seg_len).rem_euclid(pu),
                                None => 0.0,
                            };
                            self.active_run = Some(ClippedRun {
                                start: p0,
                                segments: vec![UserPathSeg::Line(p1)],
                                start_phase_user: start_phase,
                                starts_at_subpath_start: self.at_subpath_start && t0 == 0.0,
                                ends_at_subpath_start: false,
                                closed: false,
                            });
                        } else if let Some(ref mut run) = self.active_run {
                            run.segments.push(UserPathSeg::Line(p1));
                        }
                        if t1 < 1.0 {
                            if let Some(prev) = self.active_run.take() {
                                self.visible_runs.push(prev);
                            }
                        }
                    } else {
                        self.any_clipped = true;
                        if let Some(prev) = self.active_run.take() {
                            self.visible_runs.push(prev);
                        }
                    }
                    self.at_subpath_start = false;
                    if let Some(pu) = self.period_user {
                        self.phase = (self.phase + seg_len).rem_euclid(pu);
                    }
                    Ok(())
                }

                fn push_cubic(
                    &mut self,
                    p0: Point,
                    c1: Point,
                    c2: Point,
                    p3: Point,
                    depth: u8,
                    nodes_left: &mut usize,
                ) -> Result<(), ()> {
                    let min_x = p0.x.min(c1.x).min(c2.x).min(p3.x);
                    let max_x = p0.x.max(c1.x).max(c2.x).max(p3.x);
                    let min_y = p0.y.min(c1.y).min(c2.y).min(p3.y);
                    let max_y = p0.y.max(c1.y).max(c2.y).max(p3.y);
                    if max_x < self.vis_user.x0
                        || min_x > self.vis_user.x1
                        || max_y < self.vis_user.y0
                        || min_y > self.vis_user.y1
                    {
                        self.any_clipped = true;
                        if let Some(prev) = self.active_run.take() {
                            self.visible_runs.push(prev);
                        }
                        self.at_subpath_start = false;
                        if let Some(pu) = self.period_user {
                            let len = ContentInterpreter::cubic_arc_length(
                                p0,
                                c1,
                                c2,
                                p3,
                                self.phase_tol,
                            )
                            .ok_or(())?;
                            self.phase = (self.phase + len).rem_euclid(pu);
                        }
                        return Ok(());
                    }
                    if min_x >= self.vis_user.x0
                        && max_x <= self.vis_user.x1
                        && min_y >= self.vis_user.y0
                        && max_y <= self.vis_user.y1
                    {
                        let len = if let Some(pu) = self.period_user {
                            let l = ContentInterpreter::cubic_arc_length(
                                p0,
                                c1,
                                c2,
                                p3,
                                self.phase_tol,
                            )
                            .ok_or(())?;
                            *self.cumulative_dashes += l / pu;
                            if !self.cumulative_dashes.is_finite()
                                || *self.cumulative_dashes > self.max_dash_repeats
                            {
                                return Err(());
                            }
                            Some((l, pu))
                        } else {
                            None
                        };
                        if let Some(ref mut run) = self.active_run {
                            run.segments.push(UserPathSeg::Cubic(c1, c2, p3));
                        } else {
                            self.active_run = Some(ClippedRun {
                                start: p0,
                                segments: vec![UserPathSeg::Cubic(c1, c2, p3)],
                                start_phase_user: self.phase,
                                starts_at_subpath_start: self.at_subpath_start,
                                ends_at_subpath_start: false,
                                closed: false,
                            });
                        }
                        self.at_subpath_start = false;
                        if let Some((l, pu)) = len {
                            self.phase = (self.phase + l).rem_euclid(pu);
                        }
                        return Ok(());
                    }
                    if depth < 20 && *nodes_left > 0 {
                        *nodes_left -= 1;
                        let (left, right) = ContentInterpreter::split_cubic_half(p0, c1, c2, p3);
                        self.push_cubic(left[0], left[1], left[2], left[3], depth + 1, nodes_left)?;
                        self.push_cubic(
                            right[0],
                            right[1],
                            right[2],
                            right[3],
                            depth + 1,
                            nodes_left,
                        )
                    } else {
                        self.push_line(p0, p3)
                    }
                }
            }

            let dashes_before_sp = cumulative_dashes;
            let mut ctx = ClipCtx {
                vis_user,
                period_user,
                phase_tol,
                max_dash_repeats: MAX_DASH_REPEATS,
                phase: initial_phase_user,
                cumulative_dashes: &mut cumulative_dashes,
                any_clipped: false,
                at_subpath_start: true,
                active_run: None,
                visible_runs: Vec::new(),
            };
            let mut cur_pt = sp.start;
            for &seg in &sp.segments {
                match seg {
                    UserPathSeg::Line(p) => {
                        if ctx.push_line(cur_pt, p).is_err() {
                            return AnisotropicStroke::Fallback;
                        }
                        cur_pt = p;
                    }
                    UserPathSeg::Cubic(c1, c2, end) => {
                        let mut nodes_left = 2048usize;
                        if ctx
                            .push_cubic(cur_pt, c1, c2, end, 0, &mut nodes_left)
                            .is_err()
                        {
                            return AnisotropicStroke::Fallback;
                        }
                        cur_pt = end;
                    }
                }
            }
            if sp.closed {
                let needs_closing_seg = cur_pt.x != sp.start.x || cur_pt.y != sp.start.y;
                if needs_closing_seg && ctx.push_line(cur_pt, sp.start).is_err() {
                    return AnisotropicStroke::Fallback;
                }
                if !ctx.any_clipped && ctx.visible_runs.is_empty() {
                    if let Some(ref mut run) = ctx.active_run {
                        if needs_closing_seg {
                            run.segments.pop();
                        }
                        run.closed = true;
                    }
                } else if let Some(ref mut run) = ctx.active_run {
                    run.ends_at_subpath_start = true;
                }
            }
            if let Some(run) = ctx.active_run.take() {
                ctx.visible_runs.push(run);
            }
            let mut visible_runs = ctx.visible_runs;
            if visible_runs.is_empty() {
                continue;
            }
            let sp_vis_dashes = (cumulative_dashes - dashes_before_sp).max(0.0);
            if dash_spec.is_some()
                && out
                    .points()
                    .len()
                    .saturating_add(min_points_for_dashes(sp_vis_dashes))
                    > remaining_points
            {
                return AnisotropicStroke::Fallback;
            }

            // If a partially clipped closed contour both starts and ends
            // unclipped at `sp.start`, preserve the closing corner join.
            if sp.closed
                && visible_runs.len() >= 2
                && visible_runs[0].starts_at_subpath_start
                && visible_runs.last().is_some_and(|r| r.ends_at_subpath_start)
            {
                let last_run = visible_runs.pop().unwrap();
                let first_run = visible_runs.remove(0);
                if dash_spec.is_some() {
                    let Ok(last_path) =
                        build_skia_path(sp.start, last_run.start, &last_run.segments, false)
                    else {
                        return AnisotropicStroke::Fallback;
                    };
                    let Ok(first_path) =
                        build_skia_path(sp.start, first_run.start, &first_run.segments, false)
                    else {
                        return AnisotropicStroke::Fallback;
                    };
                    let dashed_last = match last_path {
                        Some(p) => {
                            let Some(d) = make_skia_dash(last_run.start_phase_user) else {
                                return AnisotropicStroke::Fallback;
                            };
                            p.dash(&d, RES_SCALE)
                        }
                        None => None,
                    };
                    let dashed_first = match first_path {
                        Some(p) => {
                            let Some(d) = make_skia_dash(first_run.start_phase_user) else {
                                return AnisotropicStroke::Fallback;
                            };
                            p.dash(&d, RES_SCALE)
                        }
                        None => None,
                    };
                    match (dashed_last, dashed_first) {
                        (Some(dl), Some(df)) => {
                            if let Some(joined) = Self::join_dashed_closing_contours(&dl, &df) {
                                if !stroke_into(&joined, sp.start, &mut out) {
                                    return AnisotropicStroke::Fallback;
                                }
                            } else {
                                if !stroke_into(&dl, sp.start, &mut out)
                                    || !stroke_into(&df, sp.start, &mut out)
                                {
                                    return AnisotropicStroke::Fallback;
                                }
                            }
                        }
                        (Some(dl), None) => {
                            if !stroke_into(&dl, sp.start, &mut out) {
                                return AnisotropicStroke::Fallback;
                            }
                        }
                        (None, Some(df)) => {
                            if !stroke_into(&df, sp.start, &mut out) {
                                return AnisotropicStroke::Fallback;
                            }
                        }
                        (None, None) => {}
                    }
                } else {
                    let mut merged_segs = last_run.segments;
                    merged_segs.extend_from_slice(&first_run.segments);
                    let Ok(merged_path) =
                        build_skia_path(sp.start, last_run.start, &merged_segs, false)
                    else {
                        return AnisotropicStroke::Fallback;
                    };
                    if let Some(p) = merged_path {
                        if !stroke_into(&p, sp.start, &mut out) {
                            return AnisotropicStroke::Fallback;
                        }
                    }
                }
            }

            for run in visible_runs {
                let Ok(mini_path) =
                    build_skia_path(run.start, run.start, &run.segments, run.closed)
                else {
                    return AnisotropicStroke::Fallback;
                };
                let Some(mini_path) = mini_path else {
                    continue;
                };
                if dash_spec.is_some() {
                    let Some(skia_dash) = make_skia_dash(run.start_phase_user) else {
                        return AnisotropicStroke::Fallback;
                    };
                    let Some(dashed) = mini_path.dash(&skia_dash, RES_SCALE) else {
                        continue;
                    };
                    if !stroke_into(&dashed, run.start, &mut out) {
                        return AnisotropicStroke::Fallback;
                    }
                } else if !stroke_into(&mini_path, run.start, &mut out) {
                    return AnisotropicStroke::Fallback;
                }
            }
        }

        if out.is_empty() {
            AnisotropicStroke::Empty
        } else {
            AnisotropicStroke::Outline(out)
        }
    }

    fn append_transformed_skia_path(
        stroked: &tiny_skia_path::Path,
        ctm_post: &Matrix,
        out: &mut Path,
        max_total_points: usize,
    ) -> bool {
        let mut last = Point::zero();
        for seg in stroked.segments() {
            match seg {
                tiny_skia_path::PathSegment::MoveTo(p) => {
                    if out.points().len() >= max_total_points {
                        return false;
                    }
                    last = Point::new(p.x as f64, p.y as f64);
                    let mapped = last.transform(ctm_post);
                    if !mapped.x.is_finite() || !mapped.y.is_finite() {
                        return false;
                    }
                    out.move_to(mapped);
                }
                tiny_skia_path::PathSegment::LineTo(p) => {
                    if out.points().len() >= max_total_points {
                        return false;
                    }
                    last = Point::new(p.x as f64, p.y as f64);
                    let mapped = last.transform(ctm_post);
                    if !mapped.x.is_finite() || !mapped.y.is_finite() {
                        return false;
                    }
                    out.line_to(mapped);
                }
                tiny_skia_path::PathSegment::QuadTo(p1, p2) => {
                    if out.points().len().saturating_add(3) > max_total_points {
                        return false;
                    }
                    let q1 = Point::new(p1.x as f64, p1.y as f64);
                    let q2 = Point::new(p2.x as f64, p2.y as f64);
                    let c1 = Point::new(
                        last.x + (2.0 / 3.0) * (q1.x - last.x),
                        last.y + (2.0 / 3.0) * (q1.y - last.y),
                    )
                    .transform(ctm_post);
                    let c2 = Point::new(
                        q2.x + (2.0 / 3.0) * (q1.x - q2.x),
                        q2.y + (2.0 / 3.0) * (q1.y - q2.y),
                    )
                    .transform(ctm_post);
                    let end = q2.transform(ctm_post);
                    if ![c1.x, c1.y, c2.x, c2.y, end.x, end.y]
                        .iter()
                        .all(|v| v.is_finite())
                    {
                        return false;
                    }
                    out.curve_to(c1, c2, end);
                    last = q2;
                }
                tiny_skia_path::PathSegment::CubicTo(p1, p2, p3) => {
                    if out.points().len().saturating_add(3) > max_total_points {
                        return false;
                    }
                    let c1 = Point::new(p1.x as f64, p1.y as f64).transform(ctm_post);
                    let c2 = Point::new(p2.x as f64, p2.y as f64).transform(ctm_post);
                    let end_local = Point::new(p3.x as f64, p3.y as f64);
                    let end = end_local.transform(ctm_post);
                    if ![c1.x, c1.y, c2.x, c2.y, end.x, end.y]
                        .iter()
                        .all(|v| v.is_finite())
                    {
                        return false;
                    }
                    out.curve_to(c1, c2, end);
                    last = end_local;
                }
                tiny_skia_path::PathSegment::Close => out.close(),
            }
        }
        true
    }

    /// Liang-Barsky parametric segment clipping against an axis-aligned rectangle.
    /// Returns the visible parameter interval `(t0, t1)` in `[0.0, 1.0]`.
    fn clip_segment_to_rect(a: Point, b: Point, rect: Rect) -> Option<(f64, f64)> {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let mut t0 = 0.0f64;
        let mut t1 = 1.0f64;
        for (p, q) in [
            (-dx, a.x - rect.x0),
            (dx, rect.x1 - a.x),
            (-dy, a.y - rect.y0),
            (dy, rect.y1 - a.y),
        ] {
            if p == 0.0 {
                if q < 0.0 {
                    return None;
                }
            } else {
                let r = q / p;
                if p < 0.0 {
                    if r > t1 {
                        return None;
                    }
                    if r > t0 {
                        t0 = r;
                    }
                } else {
                    if r < t0 {
                        return None;
                    }
                    if r < t1 {
                        t1 = r;
                    }
                }
            }
        }
        Some((t0, t1))
    }

    /// Emit a pre-expanded anisotropic stroke outline using the active stroke
    /// paint, opacity, overprint, and colour space.
    fn emit_stroke_as_filled_outline(&mut self, outline: Path) {
        if cs_produces_no_marks(&self.current.stroke_cs) {
            return;
        }
        if let Some(pat) = self.current.stroke_pattern.clone() {
            match pat {
                PatternPaint::Tiling { def, base_ctm } => {
                    if self.oc_suppressed() {
                        self.type3_oc_suppressed_mark = true;
                        return;
                    }
                    if let Some(tiling) =
                        self.build_tiling_pattern_paint(&def, base_ctm, self.current.stroke_color)
                    {
                        let cmd = RenderCommand::FillPath {
                            path: outline,
                            rule: FillRule::NonZero,
                            paint: Paint::Tiling(tiling),
                            alpha: self.current.stroke_alpha,
                            overprint: None,
                        };
                        self.emit_painted(cmd);
                        return;
                    }
                }
                PatternPaint::Shading(def) => {
                    if self.oc_suppressed() {
                        self.type3_oc_suppressed_mark = true;
                        return;
                    }
                    if !self.skip_images
                        && (self.image_cache.is_some() || !self.active_type3_glyphs.is_empty())
                    {
                        if let Some(bounds) = Self::path_bounds(&outline) {
                            let clip_idx = self.display_list.commands.len();
                            self.display_list.push(RenderCommand::PushClip {
                                path: outline.clone(),
                                rule: FillRule::NonZero,
                            });
                            if self.emit_shading_image(&def, bounds) {
                                self.display_list.push(RenderCommand::PopClip);
                                return;
                            }
                            if self.display_list.commands.len() == clip_idx + 1 {
                                self.display_list.commands.pop();
                            } else {
                                self.display_list.push(RenderCommand::PopClip);
                            }
                        }
                    }
                }
            }
        }
        let paint = if self.type3_glyph && !self.current.stroke_color_explicit {
            Paint::Inherited
        } else {
            Paint::Solid(self.current.stroke_color)
        };
        let cmd = RenderCommand::FillPath {
            path: outline,
            rule: FillRule::NonZero,
            paint,
            alpha: self.current.stroke_alpha,
            overprint: self
                .overprint_descriptor(self.current.stroke_overprint, self.current.stroke_colorant),
        };
        self.emit_painted(cmd);
    }

    /// Stroke a page-space path. A stroke pattern (tiling or shading) is clipped
    /// to the stroke outline via `PushClipStroke`; everything else is a solid
    /// stroke (shading patterns fall back to the precomputed average colour when
    /// the region is degenerate).
    fn emit_stroke(&mut self, page_path: Path) {
        self.capture_stroke_rules(&page_path);
        self.stroke_page_path(page_path);
    }

    /// Paint without treating glyph contours as table-rule candidates.
    fn stroke_page_path(&mut self, page_path: Path) {
        // A `/None` (or all-`None` DeviceN) stroke colour space produces no marks.
        if cs_produces_no_marks(&self.current.stroke_cs) {
            return;
        }
        let style = self.stroke_style();
        if let Some(pat) = self.current.stroke_pattern.clone() {
            match pat {
                PatternPaint::Tiling { def, base_ctm } => {
                    if self.oc_suppressed() {
                        self.type3_oc_suppressed_mark = true;
                        return;
                    }
                    if let Some(tiling) =
                        self.build_tiling_pattern_paint(&def, base_ctm, self.current.stroke_color)
                    {
                        let cmd = RenderCommand::StrokePath {
                            path: page_path,
                            style,
                            paint: Paint::Tiling(tiling),
                            alpha: self.current.stroke_alpha,
                            overprint: None,
                        };
                        self.emit_painted(cmd);
                        return;
                    }
                }
                PatternPaint::Shading(def) => {
                    if self.oc_suppressed() {
                        self.type3_oc_suppressed_mark = true;
                        return;
                    }
                    // The stroke region is the centerline extent grown by the stroke
                    // width. Unlike `path_bounds`, a flat path (a horizontal/vertical
                    // line) is fine here — growing makes the region two-dimensional.
                    if !self.skip_images
                        && (self.image_cache.is_some() || !self.active_type3_glyphs.is_empty())
                    {
                        if let Some(bounds) = Self::stroke_region_bounds(&page_path, &style) {
                            let clip_idx = self.display_list.commands.len();
                            self.display_list.push(RenderCommand::PushClipStroke {
                                path: page_path.clone(),
                                style: style.clone(),
                            });
                            if self.emit_shading_image(&def, bounds) {
                                self.display_list.push(RenderCommand::PopClip);
                                return;
                            }
                            if self.display_list.commands.len() == clip_idx + 1 {
                                self.display_list.commands.pop();
                            } else {
                                self.display_list.push(RenderCommand::PopClip);
                            }
                        }
                    }
                }
            }
        }
        let paint = if self.type3_glyph && !self.current.stroke_color_explicit {
            Paint::Inherited
        } else {
            Paint::Solid(self.current.stroke_color)
        };
        let cmd = RenderCommand::StrokePath {
            path: page_path,
            style,
            paint,
            alpha: self.current.stroke_alpha,
            overprint: self
                .overprint_descriptor(self.current.stroke_overprint, self.current.stroke_colorant),
        };
        self.emit_painted(cmd);
    }

    /// Page-space bounding box of a stroke: the path's point extent grown by
    /// how far the page-space stroke reaches past its centerline on every side
    /// (so even a degenerate flat path yields a 2-D region for the pattern
    /// paint to cover).
    fn stroke_region_bounds(path: &Path, style: &StrokeStyle) -> Option<Rect> {
        let (mut x0, mut y0, mut x1, mut y1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        let mut acc = |p: &Point| {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        };
        path.points().iter().for_each(&mut acc);
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
            return None;
        }
        // Half the width, out to a miter tip (at most `miter_limit` half-widths;
        // the cap keeps a huge limit from inflating the region) or a square
        // cap's corner.
        let reach = match style.join {
            LineJoin::Miter => style.miter_limit.clamp(1.0, 100.0),
            _ => std::f32::consts::SQRT_2,
        };
        let grow = (style.width.max(1.0) as f64 * 0.5 * reach.max(std::f32::consts::SQRT_2) as f64)
            .max(0.5);
        Some(Rect::new(x0 - grow, y0 - grow, x1 + grow, y1 + grow))
    }

    /// Fill an already page-space path with the active fill paint — a pattern
    /// (tiling or shading) is clipped to the path; everything else is a solid
    /// fill.
    fn fill_page_path(&mut self, page_path: Path, rule: FillRule) {
        self.capture_fill_rules(&page_path);
        // A `/None` (or all-`None` DeviceN) fill colour space produces no marks.
        if cs_produces_no_marks(&self.current.fill_cs) {
            return;
        }
        if let Some(pat) = self.current.fill_pattern.clone() {
            match pat {
                PatternPaint::Tiling { def, base_ctm } => {
                    if self.oc_suppressed() {
                        self.type3_oc_suppressed_mark = true;
                        return;
                    }
                    if let Some(tiling) =
                        self.build_tiling_pattern_paint(&def, base_ctm, self.current.fill_color)
                    {
                        let cmd = RenderCommand::FillPath {
                            path: page_path,
                            rule,
                            paint: Paint::Tiling(tiling),
                            alpha: self.current.fill_alpha,
                            overprint: None,
                        };
                        self.emit_painted(cmd);
                        return;
                    }
                }
                PatternPaint::Shading(def) => {
                    if self.oc_suppressed() {
                        self.type3_oc_suppressed_mark = true;
                        return;
                    }
                    if !self.skip_images
                        && (self.image_cache.is_some() || !self.active_type3_glyphs.is_empty())
                    {
                        if let Some(bounds) = Self::path_bounds(&page_path) {
                            let push_clip = !self.clip_path_covers_current_bounds(&page_path);
                            let saved_elision_bounds = self.current.clip_elision_bounds;
                            let clip_idx = self.display_list.commands.len();
                            if push_clip {
                                self.intersect_clip_elision_bounds(
                                    self.clip_path_enforced_device_bounds(&page_path),
                                );
                                self.display_list.push(RenderCommand::PushClip {
                                    path: page_path.clone(),
                                    rule,
                                });
                            }
                            let emitted = self.emit_shading_image(&def, bounds);
                            if push_clip {
                                if !emitted && self.display_list.commands.len() == clip_idx + 1 {
                                    self.display_list.commands.pop();
                                } else {
                                    self.display_list.push(RenderCommand::PopClip);
                                }
                                self.current.clip_elision_bounds = saved_elision_bounds;
                            }
                            if emitted {
                                return;
                            }
                        }
                    }
                }
            }
        }
        let paint = if self.type3_glyph && !self.current.fill_color_explicit {
            Paint::Inherited
        } else {
            Paint::Solid(self.current.fill_color)
        };
        let cmd = RenderCommand::FillPath {
            path: page_path,
            rule,
            paint,
            alpha: self.current.fill_alpha,
            overprint: self
                .overprint_descriptor(self.current.fill_overprint, self.current.fill_colorant),
        };
        self.emit_painted(cmd);
    }

    /// Paint a pattern (tiling or shading) across `bounds`. The caller must
    /// already have installed the clip or soft-mask region. Returns `false`
    /// when the pattern could not be emitted, so the caller can fall back to
    /// a solid approximation.
    fn paint_pattern_in_region(&mut self, pattern: &PatternPaint, bounds: Rect) -> bool {
        if self.oc_suppressed() {
            self.type3_oc_suppressed_mark = true;
            return true;
        }
        match pattern {
            PatternPaint::Shading(def) => {
                if self.skip_images
                    || (self.image_cache.is_none() && self.active_type3_glyphs.is_empty())
                {
                    return false;
                }
                self.emit_shading_image(def, bounds)
            }
            PatternPaint::Tiling { def, base_ctm } => {
                let Some(tiling) =
                    self.build_tiling_pattern_paint(def, *base_ctm, self.current.fill_color)
                else {
                    return false;
                };
                let mut rect_path = Path::new();
                rect_path.rect(Rect::new(
                    bounds.x0.floor() - 1.0,
                    bounds.y0.floor() - 1.0,
                    bounds.x1.ceil() + 1.0,
                    bounds.y1.ceil() + 1.0,
                ));
                let cmd = RenderCommand::FillPath {
                    path: rect_path,
                    rule: FillRule::NonZero,
                    paint: Paint::Tiling(tiling),
                    alpha: self.current.fill_alpha,
                    overprint: None,
                };
                self.emit_painted(cmd);
                true
            }
        }
    }

    /// Pre-interpret a tiling pattern's single cell in pattern space into a
    /// reusable [`TilingPattern`] paint (ISO 32000-1 §8.7.3). Returns `None`
    /// when the pattern transform is singular, cyclic, or exceeds nesting depth,
    /// in which case callers fall back to a solid approximation.
    fn build_tiling_pattern_paint(
        &mut self,
        def: &TilingPatternDef,
        pattern_base_ctm: Matrix,
        pattern_color: Color,
    ) -> Option<std::sync::Arc<TilingPattern>> {
        const MAX_FORM_DEPTH: u32 = 16;
        if self.form_depth >= MAX_FORM_DEPTH {
            self.nesting_limit_hit = true;
            self.stats.diagnostics.resource_limit_reached = true;
            tracing::warn!("tiling pattern nesting exceeds {MAX_FORM_DEPTH}; skipping");
            return None;
        }
        if self.active_tiling_patterns.contains(&def.pat_id) {
            self.nesting_limit_hit = true;
            self.stats.diagnostics.malformed_content =
                self.stats.diagnostics.malformed_content.saturating_add(1);
            tracing::warn!(
                "tiling pattern {:?} references itself; skipping recursion",
                def.pat_id
            );
            return None;
        }
        // Pattern space is anchored to the defining stream's default user space
        // (`pattern_base_ctm`), not the CTM at fill/stroke time (ISO 32000-1 §8.7.2).
        let to_page = pattern_base_ctm.concat(&def.matrix);
        let from_page = if ![
            to_page.a, to_page.b, to_page.c, to_page.d, to_page.e, to_page.f,
        ]
        .iter()
        .all(|v| v.is_finite())
        {
            return None;
        } else {
            to_page.inverse()?
        };

        // Compute the effective device scale the CPU backend will use when
        // rasterizing this cell so image downscaling matches `CpuRenderer`
        // even when `to_page` scales/rotates/shears pattern space.
        let cell_downscale_scale = if self.image_downscale_scale > 0.0 {
            let s = self.image_downscale_scale as f64;
            let abs_x = def.x_step.abs();
            let abs_y = def.y_step.abs();
            let len_x = (s * to_page.a * abs_x).hypot(s * to_page.b * abs_x);
            let len_y = (s * to_page.c * abs_y).hypot(s * to_page.d * abs_y);
            if len_x.is_finite() && len_y.is_finite() && len_x > 0.0 && len_y > 0.0 {
                let mut tw = len_x.round().max(1.0);
                let mut th = len_y.round().max(1.0);
                const MAX_TILE_DIM: f64 = 4096.0;
                const MAX_TILE_PIXELS: f64 = 4_194_304.0;
                let max_dim = tw.max(th);
                if max_dim > MAX_TILE_DIM {
                    let k = MAX_TILE_DIM / max_dim;
                    tw = (tw * k).floor().max(1.0);
                    th = (th * k).floor().max(1.0);
                }
                if tw * th > MAX_TILE_PIXELS {
                    let k = (MAX_TILE_PIXELS / (tw * th)).sqrt();
                    tw = (tw * k).floor().max(1.0);
                    th = (th * k).floor().max(1.0);
                }
                let eff = ((tw / abs_x) as f32).max((th / abs_y) as f32);
                if eff.is_finite() && eff > 0.0 {
                    eff
                } else {
                    0.0
                }
            } else {
                0.0
            }
        } else {
            0.0
        };

        let cell_to_page = self.shading_space_to_page.concat(&to_page);
        let shading_scale = [
            (cell_to_page.a.hypot(cell_to_page.b) as f32).to_bits(),
            (cell_to_page.c.hypot(cell_to_page.d) as f32).to_bits(),
        ];

        let uncolored = def.paint_type == 2;

        // Pattern-cell initial graphics state inherits the parent stream's
        // entry state (ISO 32000-1 §8.7.3.1), with CTM in pattern space, fresh
        // clip state, and transparency/pattern-paint fields reset (§11.6.7).
        let mut cell_state = self.base_state.clone();
        cell_state.ctm = Matrix::identity();
        cell_state.clip_depth = 0;
        cell_state.clip_bounds = None;
        cell_state.clip_elision_bounds = None;
        cell_state.blend_mode = BlendMode::Normal;
        cell_state.fill_alpha = 1.0;
        cell_state.stroke_alpha = 1.0;
        cell_state.soft_mask = None;
        cell_state.fill_pattern = None;
        cell_state.stroke_pattern = None;
        if matches!(cell_state.fill_cs, ActiveColorSpace::Pattern { .. }) {
            cell_state.fill_cs = ActiveColorSpace::DeviceGray;
        }
        if matches!(cell_state.stroke_cs, ActiveColorSpace::Pattern { .. }) {
            cell_state.stroke_cs = ActiveColorSpace::DeviceGray;
        }
        if uncolored {
            cell_state.fill_color = pattern_color;
            cell_state.stroke_color = pattern_color;
            cell_state.fill_cs = ActiveColorSpace::DeviceGray;
            cell_state.stroke_cs = ActiveColorSpace::DeviceGray;
            // Uncolored cell colour is the pattern's base-space tint, not
            // tracked as a colorant; overprint is off inside the cell.
            cell_state.fill_colorant = None;
            cell_state.stroke_colorant = None;
        }
        let cell_font_id = self.base_font_id;

        let outer_page_base_ctm = if self.form_depth == 0 {
            self.base_ctm
        } else {
            self.page_base_ctm
        };
        let rebased_page_ctm = from_page.concat(&outer_page_base_ctm);
        let outer_scope_len = self.form_resources.len();
        let rebased_form_ctms: Vec<Matrix> = self
            .form_resources
            .iter()
            .map(|(_, scope_ctm)| from_page.concat(scope_ctm))
            .collect();
        let rebased_bits_for = |scope: InheritedPatternScope| -> Option<[u64; 6]> {
            match scope {
                InheritedPatternScope::Page => Some(matrix_bits(&rebased_page_ctm)),
                InheritedPatternScope::EnclosingForm(idx) => {
                    rebased_form_ctms.get(idx).map(matrix_bits)
                }
            }
        };

        let cache_key =
            InheritedGraphicsStateKey::from_state(&cell_state, cell_font_id).map(|inherited| {
                TilingCellKey {
                    pat_id: def.pat_id,
                    uncolored_color: uncolored.then(|| {
                        [
                            pattern_color.r,
                            pattern_color.g,
                            pattern_color.b,
                            pattern_color.a,
                        ]
                        .map(f32::to_bits)
                    }),
                    image_downscale: cell_downscale_scale.to_bits(),
                    shading_scale,
                    inherited,
                    resource_scopes: self.form_resource_scopes.clone(),
                    embed_type3_inline_images: self.image_cache.is_none()
                        && !self.active_type3_glyphs.is_empty(),
                }
            });
        if let Some(hit) = cache_key.as_ref().and_then(|key| {
            self.tiling_cell_cache.get(key).and_then(|entries| {
                entries.iter().find_map(|cached| {
                    if self.form_depth.saturating_add(cached.subtree_depth) <= MAX_FORM_DEPTH
                        && cached
                            .inherited_pattern_transforms
                            .iter()
                            .all(|&(scope, bits)| rebased_bits_for(scope) == Some(bits))
                    {
                        Some((
                            std::sync::Arc::clone(&cached.commands),
                            cached.subtree_depth,
                            cached.inherited_pattern_transforms.clone(),
                        ))
                    } else {
                        None
                    }
                })
            })
        }) {
            let (commands, subtree_depth, inherited_transforms) = hit;
            for (scope, _) in inherited_transforms {
                self.record_tiling_inherited_pattern_scope(scope);
            }
            self.max_depth_reached = self
                .max_depth_reached
                .max(self.form_depth.saturating_add(subtree_depth));
            let pattern = TilingPattern {
                commands,
                bbox: def.bbox,
                x_step: def.x_step,
                y_step: def.y_step,
                matrix: to_page,
                tiling_type: def.tiling_type,
            };
            if pattern.overlap_copy_ranges().truncated {
                self.stats.diagnostics.resource_limit_reached = true;
            }
            return Some(std::sync::Arc::new(pattern));
        }

        let file = self.file?;

        // Express every enclosing Form/Type 3/Pattern resource scope's default
        // CTM in this cell's coordinate system so nested patterns inherited from
        // an enclosing scaled/translated Form are anchored once (not scaled twice).
        let mut saved_form_ctms = Vec::with_capacity(outer_scope_len);
        for (entry, rebased) in self.form_resources.iter_mut().zip(rebased_form_ctms.iter()) {
            saved_form_ctms.push(entry.1);
            entry.1 = *rebased;
        }

        // Pattern /Resources and fonts shadow the page's for the cell body,
        // exactly like a form XObject's.
        self.load_form_fonts(&def.dict, Some(def.pat_id), file);
        let own_resources = def.dict.get("Resources").and_then(|res_obj| {
            self.resolve_dict_of(res_obj)
                .and_then(|d| zpdf_document::page::parse_resource_dict(&d, file).ok())
                .map(|res| (res, self.resource_scope_id(res_obj, Some(def.pat_id))))
        });
        let pushed_resources = own_resources.is_some();
        if let Some((res, scope_id)) = own_resources {
            self.form_resources
                .push((std::sync::Arc::new(res), Matrix::identity()));
            self.form_resource_scopes.push(scope_id);
        }

        // Interpret the cell once in pattern space into a detached display list
        // bounded by `/BBox`, mirroring `build_soft_mask`. Charge outer commands
        // to `retained_detached_commands` while the cell runs so `over_budget()`
        // enforces `max_commands` across both outer and retained detached cells.
        let saved_dl = std::mem::replace(&mut self.display_list, DisplayList::new(def.bbox));
        let outer_cmd_count = saved_dl.commands.len();
        self.retained_detached_commands = self
            .retained_detached_commands
            .saturating_add(outer_cmd_count);
        let depth_floor = self.state_stack.len();
        self.state_stack.push(self.current.clone());
        let saved_floor = self.state_floor;
        self.state_floor = depth_floor + 1;
        let saved_text_active = std::mem::replace(&mut self.text_active, false);
        let saved_text_matrix = self.text_matrix;
        let saved_text_line_matrix = self.text_line_matrix;
        let saved_text_clip_accum = self.text_clip_accum.take();
        let saved_text_clip_requested = std::mem::replace(&mut self.text_clip_requested, false);
        let saved_text_clip_unavailable = std::mem::replace(&mut self.text_clip_unavailable, false);
        let saved_current_font_id = self.current_font_id;
        let saved_operand_stack = std::mem::take(&mut self.operand_stack);
        let saved_current_path = std::mem::take(&mut self.current_path);
        let saved_pending_clip = self.pending_clip.take();
        let saved_suppress = self.suppress_color_ops;
        // Pattern-cell glyphs and geometry are painting resources, never logical
        // page text or table rules.
        let saved_text_sink = self.text_sink.take();
        let saved_span_streams = self.span_streams.take();
        let saved_rule_sink = self.rule_sink.take();
        let saved_active_actual_text = self.active_actual_text.take();
        let saved_mc_depth = self.mc_depth;
        let saved_mc_stack = std::mem::take(&mut self.mc_stack);
        let saved_mc_stream = self.current_mc_stream.take();
        let saved_enclosing_struct_actual_text = self.enclosing_struct_actual_text;
        let saved_compatibility = std::mem::replace(&mut self.compatibility_depth, 0);
        let saved_oc_hidden = self.oc_hidden_from.take();
        let saved_type3_inline_ctx = self.type3_inline_ctx.take();
        let saved_base_ctm = self.base_ctm;
        let saved_page_base_ctm = self.page_base_ctm;
        let saved_base_state = self.base_state.clone();
        let saved_base_font_id = self.base_font_id;
        let saved_shading_space_to_page =
            std::mem::replace(&mut self.shading_space_to_page, cell_to_page);
        // Soft masks built inside the cell live in pattern-cell space (`def.bbox`),
        // separate from the outer page's soft-mask cache.
        let saved_soft_mask_cache = std::mem::take(&mut self.soft_mask_cache);
        let saved_image_downscale_scale =
            std::mem::replace(&mut self.image_downscale_scale, cell_downscale_scale);
        let entry_depth = self.form_depth;
        let saved_nesting_limit_hit = std::mem::replace(&mut self.nesting_limit_hit, false);
        let saved_max_depth_reached = std::mem::replace(&mut self.max_depth_reached, entry_depth);

        // Pattern-cell initial graphics state inherited from parent stream entry.
        self.current = cell_state.clone();
        self.base_state = cell_state;
        self.base_font_id = cell_font_id;
        self.current_font_id = cell_font_id;
        self.text_matrix = Matrix::identity();
        self.text_line_matrix = Matrix::identity();
        self.base_ctm = Matrix::identity();
        self.page_base_ctm = rebased_page_ctm;
        self.suppress_color_ops = uncolored;

        // Clip the cell to its /BBox.
        let mut bbox_path = Path::new();
        bbox_path.rect(def.bbox);
        self.display_list.push(RenderCommand::PushClip {
            path: bbox_path,
            rule: FillRule::NonZero,
        });

        self.form_depth += 1;
        self.max_depth_reached = self.max_depth_reached.max(self.form_depth);
        self.active_tiling_patterns.push(def.pat_id);
        self.tiling_inherited_pattern_scopes
            .push((outer_scope_len, Vec::new()));
        let mut truncated = false;
        let mut tokenizer = ContentTokenizer::new(&def.content);
        while let Some(token) = tokenizer.next_borrowed_recovering() {
            if self.over_budget() {
                truncated = true;
                break;
            }
            match token {
                ContentToken::Operand(obj) => {
                    if !self.push_operand(obj) {
                        truncated = true;
                        break;
                    }
                }
                ContentToken::Operator(op) => {
                    self.execute_operator(&op);
                    self.operand_stack.clear();
                }
                ContentToken::InlineImage { dict, data } => {
                    self.do_inline_image(dict, data);
                    self.operand_stack.clear();
                }
            }
        }
        self.stats.diagnostics.merge(tokenizer.diagnostics());
        self.close_marked_content();
        let used_inherited_scopes = self
            .tiling_inherited_pattern_scopes
            .pop()
            .map(|(_, s)| s)
            .unwrap_or_default();
        self.active_tiling_patterns.pop();
        self.form_depth -= 1;

        let descendant_nesting_hit = self.nesting_limit_hit;
        let subtree_depth = self.max_depth_reached.saturating_sub(entry_depth).max(1);
        self.nesting_limit_hit = saved_nesting_limit_hit || descendant_nesting_hit;
        self.max_depth_reached = saved_max_depth_reached.max(self.max_depth_reached);

        // Rebalance clips/state left open by the cell body.
        while self.state_stack.len() > depth_floor + 1 {
            for _ in 0..self.current.clip_depth {
                self.display_list.push(RenderCommand::PopClip);
            }
            if let Some(state) = self.state_stack.pop() {
                self.current = state;
            }
        }
        for _ in 0..self.current.clip_depth {
            self.display_list.push(RenderCommand::PopClip);
        }
        if let Some(state) = self.state_stack.pop() {
            self.current = state;
        }
        if self.display_list.commands.len() <= 1 {
            // Cell painted nothing (e.g. empty stream or hidden OC layer):
            // drop the no-op bbox clip so backends can fast-skip it.
            self.display_list.commands.clear();
        } else {
            self.display_list.push(RenderCommand::PopClip);
        }

        let cell_dl = std::mem::replace(&mut self.display_list, saved_dl);
        self.retained_detached_commands = self
            .retained_detached_commands
            .saturating_sub(outer_cmd_count)
            .saturating_add(cell_dl.commands.len());
        self.state_floor = saved_floor;
        self.text_active = saved_text_active;
        self.text_matrix = saved_text_matrix;
        self.text_line_matrix = saved_text_line_matrix;
        self.text_clip_accum = saved_text_clip_accum;
        self.text_clip_requested = saved_text_clip_requested;
        self.text_clip_unavailable = saved_text_clip_unavailable;
        self.current_font_id = saved_current_font_id;
        self.operand_stack = saved_operand_stack;
        self.current_path = saved_current_path;
        self.pending_clip = saved_pending_clip;
        self.suppress_color_ops = saved_suppress;
        self.text_sink = saved_text_sink;
        self.span_streams = saved_span_streams;
        self.rule_sink = saved_rule_sink;
        self.active_actual_text = saved_active_actual_text;
        self.mc_depth = saved_mc_depth;
        self.mc_stack = saved_mc_stack;
        self.current_mc_stream = saved_mc_stream;
        self.enclosing_struct_actual_text = saved_enclosing_struct_actual_text;
        self.compatibility_depth = saved_compatibility;
        self.oc_hidden_from = saved_oc_hidden;
        self.type3_inline_ctx = saved_type3_inline_ctx;
        self.base_ctm = saved_base_ctm;
        self.page_base_ctm = saved_page_base_ctm;
        self.base_state = saved_base_state;
        self.base_font_id = saved_base_font_id;
        self.shading_space_to_page = saved_shading_space_to_page;
        self.soft_mask_cache = saved_soft_mask_cache;
        self.image_downscale_scale = saved_image_downscale_scale;
        self.pop_form_fonts();
        if pushed_resources {
            self.form_resources.pop();
            self.form_resource_scopes.pop();
        }
        for (entry, saved) in self.form_resources.iter_mut().zip(saved_form_ctms) {
            entry.1 = saved;
        }

        let commands = std::sync::Arc::new(cell_dl);
        if let Some(cache_key) = cache_key {
            if !truncated
                && !descendant_nesting_hit
                && self.tiling_cell_cache_entries < MAX_CACHED_TILING_CELLS
            {
                let inherited_pattern_transforms: Vec<(InheritedPatternScope, [u64; 6])> =
                    used_inherited_scopes
                        .into_iter()
                        .filter_map(|scope| rebased_bits_for(scope).map(|bits| (scope, bits)))
                        .collect();
                let bucket = self.tiling_cell_cache.entry(cache_key).or_default();
                if bucket.len() < MAX_CACHED_TILING_VARIANTS_PER_KEY {
                    bucket.push(CachedTilingCell {
                        commands: std::sync::Arc::clone(&commands),
                        subtree_depth,
                        inherited_pattern_transforms,
                    });
                    self.tiling_cell_cache_entries += 1;
                }
            }
        }
        let pattern = TilingPattern {
            commands,
            bbox: def.bbox,
            x_step: def.x_step,
            y_step: def.y_step,
            matrix: to_page,
            tiling_type: def.tiling_type,
        };
        if pattern.overlap_copy_ranges().truncated {
            self.stats.diagnostics.resource_limit_reached = true;
            tracing::warn!(
                "tiling pattern {:?} overlapping cell copies exceed {}; clamping",
                def.pat_id,
                zpdf_display_list::MAX_TILING_OVERLAP_COPIES
            );
        }
        Some(std::sync::Arc::new(pattern))
    }

    /// Emit a glyph run with an explicit paint (solid colour or `Paint::Tiling`
    /// for outline-less Type 3 glyphs).
    fn emit_glyph_run_with_paint(
        &mut self,
        font_id: FontId,
        font_size: f32,
        glyphs: Vec<PositionedGlyph>,
        transform: Matrix,
        h_scale: f32,
        paint: Paint,
    ) {
        let overprint =
            self.overprint_descriptor(self.current.fill_overprint, self.current.fill_colorant);
        let cmd = RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size,
            glyphs,
            paint,
            alpha: self.current.fill_alpha,
            overprint,
            transform,
            h_scale,
        });
        self.emit_painted(cmd);
    }

    /// Emit a solid-colour glyph run (the non-pattern path, and the fallback
    /// when a tiling text pattern cannot be replicated). Type 3 fonts are
    /// expanded directly in `ContentInterpreter` (honoring any active fill
    /// pattern, `d0`/`d1`, and glyph/font/parent `/Resources`).
    fn emit_solid_glyph_run(
        &mut self,
        font_id: FontId,
        font_size: f32,
        glyphs: Vec<PositionedGlyph>,
        transform: Matrix,
        h_scale: f32,
    ) {
        let is_type3 = self
            .font_cache
            .as_ref()
            .and_then(|fc| fc.get(font_id))
            .is_some_and(|font| font.is_type3());
        if is_type3 {
            if self.skip_type3_painting {
                return;
            }
            let paint_color = self.current.fill_color;
            let alpha = self.current.fill_alpha;
            let overprint =
                self.overprint_descriptor(self.current.fill_overprint, self.current.fill_colorant);
            self.paint_type3_glyph_run(
                font_id,
                font_size,
                &glyphs,
                transform,
                h_scale,
                paint_color,
                alpha,
                overprint,
            );
            return;
        }
        // GlyphRun is a fill operation. Stroke modes use actual outline paths.
        let paint_color = self.current.fill_color;
        self.emit_glyph_run_with_paint(
            font_id,
            font_size,
            glyphs,
            transform,
            h_scale,
            Paint::Solid(paint_color),
        );
    }

    /// Resolve an inline Type 3 font dictionary stored under `/Resources /Font`
    /// of an indirect container object (`owner_id`).
    fn resolve_container_inline_font_dict(
        &self,
        file: &PdfFile,
        owner_id: ObjectId,
        font_name: &str,
    ) -> Option<zpdf_core::PdfDict> {
        let owner_obj = file.resolve_shared(owner_id).ok()?;
        let owner_dict = match &*owner_obj {
            PdfObject::Dict(d) => d,
            PdfObject::Stream(s) => &s.dict,
            _ => return None,
        };
        let res_dict = self.resolve_dict_of(owner_dict.get("Resources")?)?;
        let fonts_dict = self.resolve_dict_of(res_dict.get("Font")?)?;
        match fonts_dict.get(font_name)? {
            PdfObject::Dict(d) => Some(d.clone()),
            PdfObject::Ref(r) => file.resolve(*r).ok()?.as_dict().ok().cloned(),
            _ => None,
        }
    }

    /// Resolve an inline Type 3 font dictionary stored directly under `/Font`
    /// of an indirect `/Resources` dictionary object (`resources_id`).
    fn resolve_resources_inline_font_dict(
        &self,
        file: &PdfFile,
        resources_id: ObjectId,
        font_name: &str,
    ) -> Option<zpdf_core::PdfDict> {
        let res_obj = file.resolve_shared(resources_id).ok()?;
        let res_dict = res_obj.as_dict().ok()?;
        let fonts_dict = self.resolve_dict_of(res_dict.get("Font")?)?;
        match fonts_dict.get(font_name)? {
            PdfObject::Dict(d) => Some(d.clone()),
            PdfObject::Ref(r) => file.resolve(*r).ok()?.as_dict().ok().cloned(),
            _ => None,
        }
    }

    fn reserve_type3_resource_cache(&mut self, bytes: usize) -> bool {
        let total = self.type3_resource_cache_bytes.saturating_add(bytes);
        if total > self.max_type3_resource_cache_bytes {
            return false;
        }
        self.type3_resource_cache_bytes = total;
        true
    }

    /// Parse a `/Resources` object for a Type 3 font or CharProc, sharing a
    /// single parsed `Arc<CachedType3ResourceScope>` across all callers that
    /// reference the same indirect `/Resources` object ID and bounding retained
    /// bytes for distinct scopes.
    ///
    /// Returns `(scope, direct_cache_cost)`: `Some(0)` when an indirect scope is
    /// already owned by `type3_shared_resources`, `Some(cost)` when a direct
    /// scope must be reserved atomically alongside its owning cache entry, and
    /// `None` when the indirect scope exceeded the cache cap.
    fn load_type3_resource_scope(
        &mut self,
        res_obj: &PdfObject,
        container_owner_id: Option<ObjectId>,
        file: &PdfFile,
    ) -> Option<(std::sync::Arc<CachedType3ResourceScope>, Option<usize>)> {
        if let PdfObject::Ref(res_id) = res_obj {
            if let Some(shared) = self.type3_shared_resources.get(res_id) {
                return Some((std::sync::Arc::clone(shared), Some(0)));
            }
            let res_dict = self.resolve_dict_of(res_obj)?;
            let parsed_resources = zpdf_document::page::parse_resource_dict(&res_dict, file)
                .ok()
                .map(std::sync::Arc::new);
            let font_bindings = self.prepare_font_bindings(
                &res_dict,
                Some(InlineFontOwner::ResourcesDict(*res_id)),
                file,
            );
            let scope = std::sync::Arc::new(CachedType3ResourceScope {
                scope_id: ResourceScopeId::ResourcesObject(*res_id),
                parsed_resources,
                font_bindings,
            });
            let cost = estimate_type3_resource_scope_bytes(&scope).saturating_add(64);
            let cache_cost = if self.reserve_type3_resource_cache(cost) {
                self.type3_shared_resources
                    .insert(*res_id, std::sync::Arc::clone(&scope));
                Some(0)
            } else {
                None
            };
            return Some((scope, cache_cost));
        }

        let res_dict = self.resolve_dict_of(res_obj)?;
        let parsed_resources = zpdf_document::page::parse_resource_dict(&res_dict, file)
            .ok()
            .map(std::sync::Arc::new);
        let font_bindings = self.prepare_font_bindings(
            &res_dict,
            container_owner_id.map(InlineFontOwner::Container),
            file,
        );
        let scope_id = match container_owner_id {
            Some(owner_id) => ResourceScopeId::OwnerObject(owner_id),
            None => {
                self.next_resource_scope_id = self.next_resource_scope_id.wrapping_add(1);
                ResourceScopeId::Unique(self.next_resource_scope_id)
            }
        };
        let scope = std::sync::Arc::new(CachedType3ResourceScope {
            scope_id,
            parsed_resources,
            font_bindings,
        });
        let cost = estimate_type3_resource_scope_bytes(&scope);
        Some((scope, Some(cost)))
    }

    /// Parse and cache the font-level `/Resources` scope, pre-loaded font
    /// bindings, and `/CharProcs` stream object IDs once per Type 3 font on
    /// this page.
    fn cached_type3_font_resources(
        &mut self,
        font_id: FontId,
    ) -> std::sync::Arc<CachedType3FontResources> {
        if let Some(cached) = self.type3_font_resources.get(&font_id) {
            return std::sync::Arc::clone(cached);
        }
        let mut info = CachedType3FontResources::default();
        let mut direct_scope_cost: Option<usize> = Some(0);
        if let Some(file) = self.file {
            let source = self
                .font_cache
                .as_ref()
                .and_then(|fc| fc.type3_source(font_id))
                .cloned();
            let (font_dict, owner_id) = match source {
                Some(zpdf_font::Type3Source::Object(id)) => (
                    file.resolve(id)
                        .ok()
                        .and_then(|o| o.as_dict().ok().cloned()),
                    Some(id),
                ),
                Some(zpdf_font::Type3Source::ContainerInlineFont {
                    owner_id,
                    font_name,
                }) => (
                    self.resolve_container_inline_font_dict(file, owner_id, &font_name),
                    None,
                ),
                Some(zpdf_font::Type3Source::ResourcesInlineFont {
                    resources_id,
                    font_name,
                }) => (
                    self.resolve_resources_inline_font_dict(file, resources_id, &font_name),
                    None,
                ),
                Some(zpdf_font::Type3Source::Inline(dict)) => (Some((*dict).clone()), None),
                Some(zpdf_font::Type3Source::Unavailable) | None => (None, None),
            };
            if let Some(dict) = font_dict {
                if let Some(res_obj) = dict.get("Resources") {
                    if let Some((scope, scope_cost)) =
                        self.load_type3_resource_scope(res_obj, owner_id, file)
                    {
                        info.scope = Some(scope);
                        direct_scope_cost = scope_cost;
                    }
                }
                if let Some(cp_obj) = dict.get("CharProcs") {
                    if let Some(cp_dict) = self.resolve_dict_of(cp_obj) {
                        for (name, val) in &cp_dict.0 {
                            if let PdfObject::Ref(r) = val {
                                info.char_proc_ids.insert(name.0.clone(), *r);
                            }
                        }
                    }
                }
            }
        }
        let map_cost = info.char_proc_ids.keys().fold(64usize, |acc, k| {
            acc.saturating_add(48).saturating_add(k.len())
        });
        let arc = std::sync::Arc::new(info);
        if let Some(scope_cost) = direct_scope_cost {
            let total_cost = map_cost.saturating_add(scope_cost);
            if self.reserve_type3_resource_cache(total_cost) {
                self.type3_font_resources
                    .insert(font_id, std::sync::Arc::clone(&arc));
            }
        }
        arc
    }

    /// Parse and cache a CharProc stream's optional `/Resources` scope and
    /// font bindings, sharing parsed indirect `/Resources` scopes across
    /// CharProc streams and bounding retention for distinct scopes.
    fn cached_type3_glyph_resources(
        &mut self,
        cp_id: ObjectId,
    ) -> Option<std::sync::Arc<CachedType3ResourceScope>> {
        if let Some(cached) = self.type3_glyph_resources.get(&cp_id) {
            return cached.as_ref().map(std::sync::Arc::clone);
        }
        let file = self.file?;
        let res_obj = file.resolve_shared(cp_id).ok().and_then(|obj| {
            obj.as_stream()
                .ok()
                .and_then(|s| s.dict.get("Resources").cloned())
        });
        let Some(res_obj) = res_obj else {
            if self.reserve_type3_resource_cache(48) {
                self.type3_glyph_resources.insert(cp_id, None);
            }
            return None;
        };
        let Some((scope, scope_cost)) = self.load_type3_resource_scope(&res_obj, Some(cp_id), file)
        else {
            if self.reserve_type3_resource_cache(48) {
                self.type3_glyph_resources.insert(cp_id, None);
            }
            return None;
        };
        if let Some(direct_cost) = scope_cost {
            if self.reserve_type3_resource_cache(48usize.saturating_add(direct_cost)) {
                self.type3_glyph_resources
                    .insert(cp_id, Some(std::sync::Arc::clone(&scope)));
            }
        }
        Some(scope)
    }

    /// Interpret each Type 3 glyph's CharProc content stream directly in
    /// `ContentInterpreter` (ISO 32000-1 §9.6.5), with resource lookup ordered
    /// glyph → font → parent, honoring `d0` (coloured) vs `d1` (uncoloured),
    /// and emitting the full `RenderCommand` set across all backends.
    #[allow(clippy::too_many_arguments)]
    fn paint_type3_glyph_run(
        &mut self,
        font_id: FontId,
        font_size: f32,
        glyphs: &[PositionedGlyph],
        transform: Matrix,
        h_scale: f32,
        paint_color: Color,
        alpha: f32,
        overprint: Option<Overprint>,
    ) {
        const MAX_TYPE3_DEPTH: u32 = 16;
        if self.skip_type3_painting || glyphs.is_empty() || self.over_budget() {
            return;
        }
        if self.oc_suppressed() {
            self.type3_oc_suppressed_mark = true;
            return;
        }
        if self.form_depth >= MAX_TYPE3_DEPTH {
            self.stats.diagnostics.resource_limit_reached = true;
            tracing::warn!("Type 3 glyph nesting exceeds {MAX_TYPE3_DEPTH}; skipping");
            return;
        }
        if matches!(
            self.font_cache
                .as_ref()
                .and_then(|fc| fc.type3_source(font_id)),
            Some(zpdf_font::Type3Source::Unavailable)
        ) {
            self.stats.diagnostics.resource_limit_reached = true;
            tracing::warn!(
                "Type 3 font {font_id} resources could not be retained within budget; skipping replay"
            );
            return;
        }

        let record_semantic =
            self.active_type3_glyphs.is_empty() && self.active_tiling_patterns.is_empty();
        let caller_no_marks = cs_produces_no_marks(&self.current.fill_cs);
        let font_meta = self.cached_type3_font_resources(font_id);
        let mut pushed_font_res = false;
        let mut font_res_idx = None;
        let mut pushed_font_overrides = false;
        if let Some(scope) = font_meta.scope.as_ref() {
            if let Some(res) = scope.parsed_resources.as_ref() {
                font_res_idx = Some(self.form_resources.len());
                self.form_resources
                    .push((std::sync::Arc::clone(res), self.base_ctm));
                self.form_resource_scopes.push(scope.scope_id);
                pushed_font_res = true;
            }
            if !scope.font_bindings.is_empty() {
                self.push_cached_font_bindings(&scope.font_bindings);
                pushed_font_overrides = true;
            }
        }

        let mut completed = true;
        let mut verified_segments: Vec<(usize, usize, Vec<PositionedGlyph>)> = Vec::new();
        let mut active_verified: Option<(usize, usize, Vec<PositionedGlyph>)> = None;

        for glyph in glyphs {
            if self.over_budget() {
                completed = false;
                break;
            }
            let mut verified_text = String::new();
            let glyph_verified = record_semantic
                && self
                    .font_cache
                    .as_ref()
                    .and_then(|fc| fc.get(font_id))
                    .is_some_and(|font| {
                        font.append_verified_type3_unicode(
                            glyph.code,
                            glyph.code_len,
                            &mut verified_text,
                        )
                    });
            let is_whitespace_glyph = glyph_verified
                && !verified_text.is_empty()
                && verified_text.chars().all(char::is_whitespace);
            let Some((glyph_name, stream_bytes, font_matrix)) = self
                .font_cache
                .as_ref()
                .and_then(|fc| fc.get(font_id))
                .and_then(|font| font.type3_glyph_entry(glyph.glyph_id))
            else {
                if is_whitespace_glyph && !caller_no_marks {
                    let pos = self.display_list.commands.len();
                    match active_verified.as_mut() {
                        Some((_, end, seg_glyphs)) if *end == pos => seg_glyphs.push(*glyph),
                        _ => {
                            if let Some(prev) = active_verified.take() {
                                verified_segments.push(prev);
                            }
                            active_verified = Some((pos, pos, vec![*glyph]));
                        }
                    }
                } else if let Some(prev) = active_verified.take() {
                    verified_segments.push(prev);
                }
                continue;
            };
            if self
                .active_type3_glyphs
                .contains(&(font_id, glyph.glyph_id))
            {
                self.stats.diagnostics.malformed_content =
                    self.stats.diagnostics.malformed_content.saturating_add(1);
                tracing::warn!(
                    "Type 3 glyph ({font_id}, {}) draws itself; skipping recursion",
                    glyph.glyph_id
                );
                if let Some(prev) = active_verified.take() {
                    verified_segments.push(prev);
                }
                continue;
            }

            let glyph_ctm = transform
                .concat(&Matrix::translate(glyph.x as f64, glyph.y as f64))
                .concat(&Matrix::scale(
                    font_size as f64 * h_scale as f64,
                    font_size as f64,
                ))
                .concat(&Matrix::new(
                    font_matrix[0],
                    font_matrix[1],
                    font_matrix[2],
                    font_matrix[3],
                    font_matrix[4],
                    font_matrix[5],
                ));
            if ![
                glyph_ctm.a,
                glyph_ctm.b,
                glyph_ctm.c,
                glyph_ctm.d,
                glyph_ctm.e,
                glyph_ctm.f,
            ]
            .iter()
            .all(|v| v.is_finite())
            {
                if let Some(prev) = active_verified.take() {
                    verified_segments.push(prev);
                }
                continue;
            }

            if let Some(idx) = font_res_idx {
                self.form_resources[idx].1 = glyph_ctm;
            }
            // Optional per-glyph `/Resources` on the CharProc stream shadows
            // the Type 3 font's `/Resources`, which in turn shadows the parent.
            let mut pushed_glyph_res = false;
            let mut pushed_glyph_overrides = false;
            if let Some(&cp_id) = font_meta.char_proc_ids.get(&glyph_name) {
                if let Some(glyph_meta) = self.cached_type3_glyph_resources(cp_id) {
                    if let Some(res) = glyph_meta.parsed_resources.as_ref() {
                        self.form_resources
                            .push((std::sync::Arc::clone(res), glyph_ctm));
                        self.form_resource_scopes.push(glyph_meta.scope_id);
                        pushed_glyph_res = true;
                    }
                    if !glyph_meta.font_bindings.is_empty() {
                        self.push_cached_font_bindings(&glyph_meta.font_bindings);
                        pushed_glyph_overrides = true;
                    }
                }
            }

            let glyph_cmd_start = self.display_list.commands.len();
            let depth_floor = self.state_stack.len();
            self.state_stack.push(self.current.clone());
            let saved_floor = self.state_floor;
            self.state_floor = depth_floor + 1;
            let saved_text_matrix = self.text_matrix;
            let saved_text_line_matrix = self.text_line_matrix;
            let saved_text_active = std::mem::replace(&mut self.text_active, false);
            let saved_text_clip_accum = self.text_clip_accum.take();
            let saved_text_clip_requested = std::mem::replace(&mut self.text_clip_requested, false);
            let saved_text_clip_unavailable =
                std::mem::replace(&mut self.text_clip_unavailable, false);
            let saved_current_font_id = self.current_font_id;
            let saved_operand_stack = std::mem::take(&mut self.operand_stack);
            let saved_current_path = std::mem::take(&mut self.current_path);
            let saved_pending_clip = self.pending_clip.take();
            let saved_compatibility = std::mem::replace(&mut self.compatibility_depth, 0);
            let saved_mc_depth = self.mc_depth;
            let saved_enclosing_struct_actual_text = self.enclosing_struct_actual_text;
            let saved_mc_stack = std::mem::take(&mut self.mc_stack);
            let saved_mc_stream = self.current_mc_stream.take();
            let saved_oc_hidden = self.oc_hidden_from;
            let saved_text_sink = self.text_sink.take();
            let saved_span_streams = self.span_streams.take();
            let saved_rule_sink = self.rule_sink.take();
            let saved_active_actual_text = self.active_actual_text.take();
            let saved_base_ctm = self.base_ctm;
            let saved_suppress_color_ops = self.suppress_color_ops;
            let saved_type3_inline_ctx =
                self.type3_inline_ctx.replace((font_id, glyph.glyph_id, 0));
            let saved_type3_oc_suppressed =
                std::mem::replace(&mut self.type3_oc_suppressed_mark, false);
            let saved_type3_image_dropped = std::mem::replace(&mut self.type3_image_dropped, false);
            let saved_type3_local_image_paint =
                std::mem::replace(&mut self.type3_local_image_paint, false);
            let saved_type3_caller_fill = self.type3_caller_fill.replace(paint_color);

            if self.form_depth == 0 {
                self.page_base_ctm = self.base_ctm;
            }
            self.current.ctm = glyph_ctm;
            self.base_ctm = glyph_ctm;
            self.current.clip_depth = 0;
            self.current.render_mode = 0;
            self.text_matrix = Matrix::identity();
            self.text_line_matrix = Matrix::identity();

            self.form_depth += 1;
            self.active_type3_glyphs.push((font_id, glyph.glyph_id));

            let mut tokenizer = ContentTokenizer::new(&stream_bytes);
            while let Some(token) = tokenizer.next_borrowed_recovering() {
                if self.over_budget() {
                    completed = false;
                    break;
                }
                match token {
                    ContentToken::Operand(obj) => {
                        if !self.push_operand(obj) {
                            completed = false;
                            break;
                        }
                    }
                    ContentToken::Operator(op) => {
                        self.execute_operator(&op);
                        self.operand_stack.clear();
                    }
                    ContentToken::InlineImage { dict, data } => {
                        self.do_inline_image(dict, data);
                        self.operand_stack.clear();
                    }
                }
            }
            if self.over_budget() {
                completed = false;
            }
            self.stats.diagnostics.merge(tokenizer.diagnostics());
            self.close_marked_content();

            self.active_type3_glyphs.pop();
            self.form_depth -= 1;

            // Balance any unclosed q-levels and clips opened inside the glyph.
            while self.state_stack.len() > depth_floor + 1 {
                for _ in 0..self.current.clip_depth {
                    self.display_list.push(RenderCommand::PopClip);
                }
                if let Some(state) = self.state_stack.pop() {
                    self.current = state;
                }
            }
            for _ in 0..self.current.clip_depth {
                self.display_list.push(RenderCommand::PopClip);
            }
            if let Some(state) = self.state_stack.pop() {
                self.current = state;
            }

            self.state_floor = saved_floor;
            self.text_matrix = saved_text_matrix;
            self.text_line_matrix = saved_text_line_matrix;
            self.text_active = saved_text_active;
            self.text_clip_accum = saved_text_clip_accum;
            self.text_clip_requested = saved_text_clip_requested;
            self.text_clip_unavailable = saved_text_clip_unavailable;
            self.current_font_id = saved_current_font_id;
            self.operand_stack = saved_operand_stack;
            self.current_path = saved_current_path;
            self.pending_clip = saved_pending_clip;
            self.compatibility_depth = saved_compatibility;
            self.mc_depth = saved_mc_depth;
            self.enclosing_struct_actual_text = saved_enclosing_struct_actual_text;
            self.mc_stack = saved_mc_stack;
            self.current_mc_stream = saved_mc_stream;
            self.oc_hidden_from = saved_oc_hidden;
            self.text_sink = saved_text_sink;
            self.span_streams = saved_span_streams;
            self.rule_sink = saved_rule_sink;
            self.active_actual_text = saved_active_actual_text;
            self.base_ctm = saved_base_ctm;
            self.suppress_color_ops = saved_suppress_color_ops;
            self.type3_inline_ctx = saved_type3_inline_ctx;
            let glyph_oc_suppressed = std::mem::replace(
                &mut self.type3_oc_suppressed_mark,
                saved_type3_oc_suppressed,
            );
            let glyph_image_dropped =
                std::mem::replace(&mut self.type3_image_dropped, saved_type3_image_dropped);
            let glyph_local_image_paint = std::mem::replace(
                &mut self.type3_local_image_paint,
                saved_type3_local_image_paint,
            );
            self.type3_caller_fill = saved_type3_caller_fill;

            if pushed_glyph_overrides {
                self.pop_form_fonts();
            }
            if pushed_glyph_res {
                self.form_resources.pop();
                self.form_resource_scopes.pop();
            }

            let glyph_cmd_end = self.display_list.commands.len();
            if self.active_type3_glyphs.is_empty() && glyph_cmd_end > glyph_cmd_start {
                self.display_list
                    .push_type3_glyph_range(glyph_cmd_start, glyph_cmd_end);
            }
            let glyph_cmds = &self.display_list.commands[glyph_cmd_start..glyph_cmd_end];
            let emitted_paint = glyph_cmds.iter().any(is_paint_command);
            let fully_hidden_by_oc = glyph_oc_suppressed && !emitted_paint;
            if fully_hidden_by_oc {
                // Propagate OC suppression to any enclosing Type 3 glyph.
                self.type3_oc_suppressed_mark = true;
            }
            if glyph_image_dropped {
                self.type3_image_dropped = true;
            }
            if glyph_local_image_paint {
                self.type3_local_image_paint = true;
            }
            let suppressed_by_none = caller_no_marks && !emitted_paint;
            let matches_caller_paint = !glyph_local_image_paint
                && glyph_cmds
                    .iter()
                    .all(|cmd| command_matches_caller_fill(cmd, paint_color, alpha, overprint));
            if completed
                && glyph_verified
                && (emitted_paint != is_whitespace_glyph)
                && !fully_hidden_by_oc
                && !suppressed_by_none
                && !glyph_image_dropped
                && matches_caller_paint
            {
                match active_verified.as_mut() {
                    Some((_, end, seg_glyphs)) if *end == glyph_cmd_start => {
                        *end = glyph_cmd_end;
                        seg_glyphs.push(*glyph);
                    }
                    _ => {
                        if let Some(prev) = active_verified.take() {
                            verified_segments.push(prev);
                        }
                        active_verified = Some((glyph_cmd_start, glyph_cmd_end, vec![*glyph]));
                    }
                }
            } else if let Some(prev) = active_verified.take() {
                verified_segments.push(prev);
            }

            if !completed {
                break;
            }
        }

        if pushed_font_overrides {
            self.pop_form_fonts();
        }
        if pushed_font_res {
            self.form_resources.pop();
            self.form_resource_scopes.pop();
        }

        if completed && record_semantic {
            if let Some(prev) = active_verified.take() {
                verified_segments.push(prev);
            }
            for (seg_start, seg_end, seg_glyphs) in verified_segments {
                self.display_list.push_expanded_glyph_run(
                    seg_start,
                    seg_end,
                    GlyphRun {
                        font_id,
                        font_size,
                        glyphs: seg_glyphs,
                        paint: Paint::Solid(paint_color),
                        alpha,
                        overprint,
                        transform,
                        h_scale,
                    },
                );
            }
        }
    }

    /// CAD output can show text under a font leaked from another text block.
    /// When a validated replacement exists, show this operator with it and
    /// return the font to restore afterwards (see [`crate::font_repair`]).
    fn leaked_font_override<'b, I>(
        &mut self,
        strings: impl Fn() -> I,
    ) -> Option<Option<zpdf_font::FontId>>
    where
        I: Iterator<Item = &'b [u8]>,
    {
        let current = self.current_font_id?;
        let cache = self.font_cache.as_deref()?;
        let target = self.font_repair.leaked_font(cache, current, strings)?;
        self.font_override = true;
        Some(self.current_font_id.replace(target))
    }

    fn restore_font(&mut self, restore: Option<Option<zpdf_font::FontId>>) {
        if let Some(previous) = restore {
            self.current_font_id = previous;
            self.font_override = false;
        }
    }

    /// Leave a Form's font scope; sibling candidates change with it.
    fn pop_form_fonts(&mut self) {
        if self
            .form_font_overrides
            .pop()
            .is_some_and(|names| !names.is_empty())
        {
            self.font_repair.scope_changed();
        }
    }

    fn show_text(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() && !self.oc_suppressed() && self.current_font_id.is_none() {
            self.stats.diagnostics.missing_resources =
                self.stats.diagnostics.missing_resources.saturating_add(1);
        }
        let tm = self.text_matrix;
        let ctm = self.current.ctm;
        // Bake the text rise (Ts) into the run transform as a text-space
        // translation so superscripts/subscripts leave the baseline.
        let rise_m = Matrix::translate(0.0, self.current.rise as f64);
        let combined = ctm.concat(&tm).concat(&rise_m);

        let font_size = self.current.font_size;
        let h_scale = self.current.h_scaling / 100.0;
        let char_spacing = self.current.char_spacing;
        let word_spacing = self.current.word_spacing;
        let replacement_len = self
            .active_actual_text
            .as_ref()
            .map(|actual| actual.span.text.len());
        let want_text =
            self.text_sink.is_some() && !self.oc_suppressed() && replacement_len != Some(0);

        let font_and_id = self.current_font_id.and_then(|fid| {
            self.font_cache
                .as_ref()
                .and_then(|fc| fc.get(fid).map(|f| (fid, f)))
        });

        // 2-byte = composite (Type0/CID) font. With no loaded font, assume a
        // simple single-byte font rather than guessing from the bytes.
        let is_two_byte = font_and_id
            .map(|(_, f)| matches!(f.font_type, zpdf_font::PdfFontType::Type0CidType2))
            .unwrap_or(false);

        let advance_divisor = font_and_id
            .map(|(_, f)| f.advance_divisor())
            .unwrap_or(1000.0);
        let scale_factor = font_size / advance_divisor as f32;

        let mut glyphs = Vec::new();
        let mut x_offset = 0.0f32;
        let mut vertical = false;
        // Vertical runs: text-space horizontal extent of the glyph column.
        let mut column = (f32::INFINITY, f32::NEG_INFINITY);

        if is_two_byte {
            // Composite font: segment codes through the /Encoding CMap
            // (Identity-H semantics when absent) and map code → CID → glyph.
            let cmap = font_and_id.and_then(|(_, f)| f.cid_cmap.as_ref());
            vertical = cmap.map(|c| c.wmode == 1).unwrap_or(false);
            let direct_gid = font_and_id.is_some_and(|(_, font)| font.uses_direct_composite_gids());
            let dw2 = font_and_id.map(|(_, f)| f.dw2).unwrap_or((880.0, -1000.0));
            let mut evidence = font_and_id
                .filter(|_| !self.font_override)
                .and_then(|(fid, _)| self.font_repair.evidence(fid));

            let mut i = 0usize;
            while i < bytes.len() {
                if text_deadline_reached(glyphs.len(), self.deadline) {
                    self.timed_out = true;
                    return;
                }
                let start = i;
                let (code, len) = match cmap {
                    Some(c) => c.next_code(&bytes[i..]),
                    None => {
                        let len = (bytes.len() - i).min(2);
                        let mut v = 0u32;
                        for &b in &bytes[i..i + len] {
                            v = (v << 8) | b as u32;
                        }
                        (v, len)
                    }
                };
                i += len.max(1);
                let raw_end = i.min(bytes.len());
                let raw_len = (raw_end - start) as u8;
                let raw_code = bytes[start..raw_end]
                    .iter()
                    .fold(0u32, |value, &byte| (value << 8) | byte as u32);
                if let Some(evidence) = evidence.as_deref_mut() {
                    evidence.insert(code);
                }

                // Resolve the glyph id, its 1/1000-unit advance, and (for CID
                // fonts in vertical mode) the per-CID /W2 vertical metric.
                let (glyph_id, adv_units, v_metric) = if let Some((_, font)) = font_and_id {
                    let cid = cmap.map(|c| c.code_to_cid(code, len as u8)).unwrap_or(code);
                    let cid = u16::try_from(cid).unwrap_or(0);
                    let glyph_id = if direct_gid {
                        font.composite_direct_gid(code, raw_code, len as u8)
                            .unwrap_or(0)
                    } else {
                        cid
                    };
                    let v_metric = if vertical {
                        font.cid_v_metric(cid)
                    } else {
                        None
                    };
                    (glyph_id, font.glyph_advance(cid), v_metric)
                } else {
                    (u16::try_from(code).unwrap_or(0), 500.0, None)
                };

                if vertical {
                    // Vertical writing (9.7.4.3): the glyph's horizontal origin
                    // sits at pen − v; the pen advances by ty = w1y·Tfs + Tc
                    // (+ Tw for 1-byte code 32) per 9.4.4 — no Th factor in
                    // vertical mode. Per-CID /W2 overrides /DW2 when present;
                    // the /DW2 default position vector is (w0/2, vy).
                    let (w1y, vx, vy) = match v_metric {
                        Some((w1y, vx, vy)) => (w1y, vx, vy),
                        None => (dw2.1, adv_units / 2.0, dw2.0),
                    };
                    let vx = vx as f32 * scale_factor;
                    let vy = vy as f32 * scale_factor;
                    let advance = w1y as f32 * scale_factor;
                    let width = adv_units as f32 * scale_factor * h_scale;
                    column = (column.0.min(-vx), column.1.max(-vx + width));
                    glyphs.push(PositionedGlyph {
                        glyph_id,
                        code_len: raw_len,
                        code: raw_code,
                        x: -vx,
                        y: x_offset - vy,
                        advance,
                    });
                    x_offset += advance + char_spacing;
                    if len == 1 && code == 32 {
                        x_offset += word_spacing;
                    }
                } else {
                    let advance = adv_units as f32 * scale_factor * h_scale;
                    glyphs.push(PositionedGlyph {
                        glyph_id,
                        code_len: raw_len,
                        code: raw_code,
                        x: x_offset,
                        y: 0.0,
                        advance,
                    });
                    // Per PDF 9.4.4, char/word spacing are inside the ·Th product.
                    x_offset += advance + char_spacing * h_scale;
                    // Word spacing applies to the single-byte code 32 only.
                    if len == 1 && code == 32 {
                        x_offset += word_spacing * h_scale;
                    }
                }
            }
        } else {
            let mut previous = None;
            // Codes resolve once per font and page, not once per show.
            if let (Some((fid, _)), false) = (font_and_id, self.font_override) {
                self.font_repair.note_simple(fid);
            }
            let mut codes = font_and_id.and_then(|(fid, _)| self.font_repair.simple_codes(fid));
            for (index, &byte) in bytes.iter().enumerate() {
                if text_deadline_reached(index, self.deadline) {
                    self.timed_out = true;
                    return;
                }
                let code = byte as u16;
                // Type3 CharProcs are addressed by character code. Other fonts
                // require an actual glyph mapping; an unmapped code is .notdef.
                let (glyph_id, advance) = if let Some((_, glyph)) =
                    previous.filter(|(previous_code, _)| *previous_code == byte)
                {
                    glyph
                } else if let Some((_, font)) = font_and_id {
                    let gid = match codes.as_deref_mut() {
                        Some(codes) => codes.glyph(font, byte),
                        None => font.code_to_gid(code),
                    };
                    let gid = gid.unwrap_or_else(|| if font.is_type3() { code } else { 0 });
                    let adv = font.simple_glyph_advance(code, gid) as f32 * scale_factor * h_scale;
                    (gid, adv)
                } else {
                    (code, font_size * 0.6 * h_scale)
                };
                // Font and text-space scale are constant within this operation.
                // Repeated codes need no repeated cmap parsing or width lookup;
                // positions and word/character spacing are still applied below.
                previous = Some((byte, (glyph_id, advance)));
                glyphs.push(PositionedGlyph {
                    glyph_id,
                    code_len: 1,
                    code: code as u32,
                    x: x_offset,
                    y: 0.0,
                    advance,
                });
                x_offset += advance + char_spacing * h_scale;
                // Word spacing applies only to the single-byte code 32.
                if byte == b' ' {
                    x_offset += word_spacing * h_scale;
                }
            }
        }

        // A blank glyph shown for visible text is a broken subset's missing
        // glyph; such text takes the resolving path below.
        let repair = self.current.render_mode != 3
            && !glyphs.is_empty()
            && font_and_id
                .is_some_and(|(fid, font)| self.font_repair.scan_blanks(fid, font, bytes, &glyphs));

        // Decode the text and compute its user-space placement for extraction,
        // while the immutable font borrow is still live.
        let mut geometry_omitted = false;
        let text_span = 'extract: {
            if !want_text {
                break 'extract None;
            }
            let font = font_and_id.map(|(_, f)| f);
            let cmap = font.and_then(|f| f.cid_cmap.as_ref());
            // Without an encoding CMap, rendering uses two-byte Identity
            // codes. Decode through ToUnicode at boundaries shared by the
            // drawn glyphs and the declared text width (including the tail).
            let fallback_cluster = font
                .filter(|_| is_two_byte && cmap.is_none())
                .and_then(|f| f.to_unicode.as_ref())
                .and_then(|map| match map.code_byte_lengths() {
                    [1] => Some((map, 2)),
                    [3] => Some((map, 6)),
                    [4] => Some((map, 4)),
                    _ => None,
                });
            let (descent, ascent) = selection_cell_extent(font);
            let mut decoded = String::new();
            let remaining_text_bytes =
                self.parse_limits.max_decoded_stream_bytes - self.extracted_text_bytes;
            let byte_limit = self
                .parse_limits
                .max_decoded_stream_bytes
                .min(MAX_TEXT_GEOMETRY_BYTES);
            let glyph_limit = byte_limit as usize / size_of::<TextGlyph>();
            let cluster_keeps_geometry = self
                .active_actual_text
                .as_ref()
                .is_none_or(|actual| !actual.has_source || !actual.span.glyphs.is_empty());
            // The allowance is charged when the run is kept (below), so a run
            // that decodes to nothing does not use it up.
            let within_allowance =
                glyphs.len() <= glyph_limit.saturating_sub(self.text_glyphs_collected);
            let wants_geometry = self.text_geometry && cluster_keeps_geometry;
            let collect_geometry = wants_geometry && within_allowance;
            geometry_omitted = wants_geometry && !within_allowance;
            let mut text_glyphs = if collect_geometry {
                Vec::with_capacity(glyphs.len())
            } else {
                Vec::new()
            };
            let mut offset = 0;
            let mut cluster_start = 0;
            let source = if collect_geometry || replacement_len.is_none() {
                glyphs.as_slice()
            } else {
                &[]
            };
            for (index, glyph) in source.iter().enumerate() {
                if text_deadline_reached(index, self.deadline) {
                    self.timed_out = true;
                    return;
                }
                let Some((code, len, raw_code, end)) =
                    next_text_code(cmap, is_two_byte, bytes, offset)
                else {
                    break;
                };
                let text_range = if let Some(len) = replacement_len {
                    0..len
                } else if let Some((map, cluster_bytes)) = fallback_cluster {
                    if offset.is_multiple_of(cluster_bytes) {
                        cluster_start = decoded.len();
                        let cluster_end = (offset + cluster_bytes).min(bytes.len());
                        map.append_fixed_width(&bytes[offset..cluster_end], &mut decoded);
                    }
                    cluster_start..decoded.len()
                } else {
                    let start = decoded.len();
                    if let Some(font) = font {
                        font.append_decoded_code(code, len as u8, raw_code, &mut decoded);
                    } else if (0x20..=0xff).contains(&code) && code != 0x7f {
                        decoded.push(char::from(code as u8));
                    }
                    start..decoded.len()
                };
                if decoded.len() as u64 > remaining_text_bytes {
                    self.stop_text_extraction();
                    break 'extract None;
                }
                offset = end;
                if !collect_geometry {
                    continue;
                }
                let origin = Point::new(glyph.x as f64, glyph.y as f64).transform(&combined);
                let (width, ax, ay) = if vertical {
                    let cid = cmap.map(|c| c.code_to_cid(code, len as u8)).unwrap_or(code);
                    let width = font
                        .map(|f| f.glyph_advance(u16::try_from(cid).unwrap_or(0)))
                        .unwrap_or(500.0) as f32
                        * scale_factor
                        * h_scale;
                    (width, 0.0, glyph.advance as f64)
                } else {
                    (glyph.advance, glyph.advance as f64, 0.0)
                };
                let x0 = glyph.x as f64;
                let x1 = x0 + width as f64;
                let y0 = glyph.y as f64 + descent * font_size as f64;
                let y1 = glyph.y as f64 + ascent * font_size as f64;
                text_glyphs.push(TextGlyph {
                    text_range,
                    origin,
                    advance: Point::new(
                        combined.a * ax + combined.c * ay,
                        combined.b * ax + combined.d * ay,
                    ),
                    quad: [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
                        .map(|(x, y)| Point::new(x, y).transform(&combined)),
                });
            }
            // The rise is already baked into `combined`.
            let start = Point::new(0.0, 0.0).transform(&combined);
            let (x, dx) = if vertical && column.0 <= column.1 {
                // A vertical run covers its glyph column across the pen travel.
                // Its pen moves along text-space y, so end.x − start.x alone is
                // zero for upright columns. Report the user-space x-extent.
                let (left, right) = (column.0 as f64, column.1 as f64);
                let travel = x_offset as f64;
                let xs = [(left, 0.0), (right, 0.0), (left, travel), (right, travel)]
                    .map(|(x, y)| Point::new(x, y).transform(&combined).x);
                let min = xs.into_iter().fold(f64::INFINITY, f64::min);
                let max = xs.into_iter().fold(f64::NEG_INFINITY, f64::max);
                (min, max - min)
            } else {
                let end = Point::new(x_offset as f64, 0.0).transform(&combined);
                (start.x, end.x - start.x)
            };
            let cscale = ((combined.a * combined.a + combined.b * combined.b).sqrt()
                + (combined.c * combined.c + combined.d * combined.d).sqrt())
                / 2.0;
            let span = TextSpan {
                text: decoded,
                x,
                y: start.y,
                size: (font_size as f64 * cscale) as f32,
                // Signed horizontal extent of the run (end.x − start.x) so the
                // extraction gap heuristic stays correct under scaling/rotation.
                advance: dx,
                // The marked-content id binding this run to a structure element.
                mcid: self.current_mcid(),
                glyphs: text_glyphs,
            };
            self.extracted_text_bytes += span.text.len() as u64;
            // A degenerate text matrix (e.g. 1e300-scale Tm under a large CTM)
            // places the run nowhere.
            span.is_finite().then_some(span)
        };

        // Preserve the ordinary solid-text path without entering fallback or
        // clipping machinery when no application fonts, retained embedded
        // companions, or unreadable embedded glyphs are active. Capture the
        // outer font ID before painting because tiling-pattern replay can
        // change `self.current_font_id` with `Tf`.
        let show_font_id = self.current_font_id;
        let has_companion = show_font_id.is_some_and(|fid| {
            self.font_cache.as_ref().is_some_and(|cache| {
                cache.has_embedded_companion_for_budget(fid, self.max_font_cache_bytes)
            })
        });
        let no_marks = match self.current.render_mode {
            1 | 5 => cs_produces_no_marks(&self.current.stroke_cs),
            2 | 6 => {
                cs_produces_no_marks(&self.current.fill_cs)
                    && cs_produces_no_marks(&self.current.stroke_cs)
            }
            _ => cs_produces_no_marks(&self.current.fill_cs),
        };
        let wants_clip = matches!(self.current.render_mode, 4..=7);
        let paints_or_clips = wants_clip
            || (!matches!(self.current.render_mode, 3 | 7) && !no_marks && !self.oc_suppressed());
        let contributes_text = self.text_sink.is_some() && !self.oc_suppressed();
        let unreadable_embedded = (paints_or_clips || contributes_text)
            && !glyphs.is_empty()
            && show_font_id
                .is_some_and(|fid| self.scan_unreadable_embedded_glyphs(fid, bytes, &glyphs));
        let ordinary = self.current.render_mode == 0
            && !self.oc_suppressed()
            && self.current.fill_pattern.is_none()
            && self
                .font_cache
                .as_ref()
                .and_then(|cache| cache.provider())
                .is_none_or(|provider| provider.generation() == 0)
            && !no_marks
            && !repair
            && !has_companion
            && !unreadable_embedded;
        if ordinary {
            if let Some(font_id) = show_font_id {
                if !glyphs.is_empty() {
                    self.check_embedded_fallback_outlines(font_id, bytes, &glyphs, None);
                    self.emit_solid_glyph_run(font_id, font_size, glyphs, combined, h_scale);
                }
            }
        } else {
            self.paint_positioned_text(
                bytes,
                glyphs,
                combined,
                font_size,
                h_scale,
                repair,
                unreadable_embedded,
            );
        }

        // Check fallback text-decoding completeness against the outer font that
        // decoded this show (even if patterned text replayed a tiling cell with
        // its own `Tf`), independently of outline recovery and render-mode
        // visibility. An invisible (render-mode-3) show contributing to
        // extraction still activates the lazy fallback check for unreadable
        // embedded glyphs without recording a missing visual outline.
        if let Some(font_id) = show_font_id {
            if unreadable_embedded && contributes_text {
                self.ensure_lazy_embedded_fallback(font_id);
            }
            self.check_embedded_fallback_text(font_id, bytes);
        }

        // Hidden optional-content layers also extract no text (emit_painted
        // already drops the glyph run; the matrix advance below still runs).
        let span_stream_binding = self.current_span_stream_binding();
        if let (Some(span), Some(sink)) = (text_span, self.text_sink.as_mut()) {
            let kept = if let Some(actual) = &mut self.active_actual_text {
                if !bytes.is_empty() {
                    actual.add_source(span, &mut self.text_glyphs_collected);
                }
                !bytes.is_empty()
            } else if !span.text.is_empty() {
                self.text_glyphs_collected += span.glyphs.len();
                sink.push(span);
                if let Some(streams) = self.span_streams.as_deref_mut() {
                    streams.push(span_stream_binding);
                }
                true
            } else {
                false
            };
            // A run that reaches no span has no geometry to miss.
            self.stats.text_geometry_omitted |= kept && geometry_omitted;
        }

        // Advance the text matrix along the writing direction.
        let advance = if vertical {
            Matrix::translate(0.0, x_offset as f64)
        } else {
            Matrix::translate(x_offset as f64, 0.0)
        };
        self.text_matrix = self.text_matrix.concat(&advance);
    }

    /// Merge load-time embedded-font fallback diagnostics once per `font_id`.
    /// Load-time `decode_failures` (such as cache-budget limits) mark the text
    /// failure tracker to prevent double counting, without suppressing outline
    /// omission checks.
    fn merge_embedded_fallback(
        &mut self,
        font_id: FontId,
    ) -> Option<zpdf_core::ProcessingDiagnostics> {
        let fallback_diag = self
            .font_cache
            .as_ref()
            .and_then(|fc| fc.embedded_fallback(font_id))?;
        if !self.reported_font_fallbacks.contains(&font_id) {
            self.reported_font_fallbacks.push(font_id);
            self.stats.diagnostics.merge(fallback_diag);
            if fallback_diag.decode_failures > 0
                && !self.reported_font_text_misses.contains(&font_id)
            {
                self.reported_font_text_misses.push(font_id);
            }
        }
        Some(fallback_diag)
    }

    /// Record embedded-font fallback diagnostics for visible/clipped runs and
    /// verify that every shown visible glyph has a drawable outline. Missing
    /// outlines record `missing_glyph_outlines` independently of text decoding.
    fn check_embedded_fallback_outlines(
        &mut self,
        font_id: FontId,
        bytes: &[u8],
        glyphs: &[PositionedGlyph],
        resolved_runs: Option<&[(FontId, Vec<PositionedGlyph>)]>,
    ) {
        if bytes.is_empty() || self.oc_suppressed() {
            return;
        }
        let Some(fallback_diag) = self.merge_embedded_fallback(font_id) else {
            return;
        };
        if self.reported_font_outline_misses.contains(&font_id) {
            return;
        }
        if !self.fallback_outlines_complete(font_id, bytes, glyphs, resolved_runs) {
            self.reported_font_outline_misses.push(font_id);
            self.stats
                .diagnostics
                .record_missing_glyph_outline(fallback_diag.first_failed_object);
        }
    }

    /// Whether any shown glyph in `glyphs` resolves to a corrupt/unreadable
    /// embedded outline record in `font_id`. Results are cached per `(font_id,
    /// glyph_id)` on this page so healthy fonts pay only a bitset lookup after
    /// their first show of each glyph ID.
    fn scan_unreadable_embedded_glyphs(
        &mut self,
        font_id: FontId,
        bytes: &[u8],
        glyphs: &[PositionedGlyph],
    ) -> bool {
        let Some(font) = self.font_cache.as_deref().and_then(|fc| fc.get(font_id)) else {
            return false;
        };
        if font.is_substitute || font.is_type3() || !font.has_font_data() {
            return false;
        }
        const MAX_TRACKED_FONTS: usize = 256;
        let slot = match self
            .embedded_glyph_health
            .iter()
            .position(|(id, _)| *id == font_id)
        {
            Some(index) => Some(index),
            None if self.embedded_glyph_health.len() < MAX_TRACKED_FONTS => {
                self.embedded_glyph_health
                    .push((font_id, EmbeddedGlyphHealth::default()));
                Some(self.embedded_glyph_health.len() - 1)
            }
            None => None,
        };
        let composite = matches!(font.font_type, zpdf_font::PdfFontType::Type0CidType2);
        let cmap = font.cid_cmap.as_ref();

        // Fast path: when every nonzero glyph in this run is already cached as
        // `Readable`, skip code segmentation altogether.
        if let Some(index) = slot {
            let health = &self.embedded_glyph_health[index].1;
            if glyphs.iter().all(|g| {
                g.glyph_id == 0 || health.get(g.glyph_id) == Some(CachedGlyphHealth::Readable)
            }) {
                return false;
            }
        }

        let mut any_unreadable = false;
        let mut to_probe: Vec<(u16, u32, u32, u8)> = Vec::new();
        let mut offset = 0usize;
        for g in glyphs {
            let (code, length, raw, end) =
                next_text_code(cmap, composite, bytes, offset).unwrap_or((0, 0, 0, bytes.len()));
            offset = end;
            if g.glyph_id == 0 {
                continue;
            }
            match slot.and_then(|index| self.embedded_glyph_health[index].1.get(g.glyph_id)) {
                Some(CachedGlyphHealth::Readable) => {}
                Some(CachedGlyphHealth::Unreadable) => any_unreadable = true,
                Some(CachedGlyphHealth::CodeDependentBlank { fallback }) => {
                    if let Some(u) =
                        font.code_dependent_blank_is_unreadable(code, raw, length as u8)
                    {
                        any_unreadable |= u;
                    } else if let Some(u) = fallback {
                        any_unreadable |= u;
                    } else {
                        to_probe.push((g.glyph_id, code, raw, length as u8));
                    }
                }
                None => to_probe.push((g.glyph_id, code, raw, length as u8)),
            }
        }
        if !to_probe.is_empty() {
            to_probe.sort_unstable();
            to_probe.dedup();
            if let Some(index) = slot {
                let health = &mut self.embedded_glyph_health[index].1;
                let mut cmap_budget = health.cmap_budget;
                font.classify_embedded_glyphs_with_codes(
                    &to_probe,
                    &mut cmap_budget,
                    |gid, out| match out {
                        zpdf_font::EmbeddedGlyphHealthOutcome::Readable => {
                            health.set(gid, CachedGlyphHealth::Readable);
                        }
                        zpdf_font::EmbeddedGlyphHealthOutcome::Unreadable => {
                            health.set(gid, CachedGlyphHealth::Unreadable);
                            any_unreadable = true;
                        }
                        zpdf_font::EmbeddedGlyphHealthOutcome::CodeDependent { unreadable } => {
                            let fallback = match health.get(gid) {
                                Some(CachedGlyphHealth::CodeDependentBlank { fallback }) => {
                                    fallback
                                }
                                _ => None,
                            };
                            health.set(gid, CachedGlyphHealth::CodeDependentBlank { fallback });
                            any_unreadable |= unreadable;
                        }
                        zpdf_font::EmbeddedGlyphHealthOutcome::CodeDependentFallback {
                            unreadable,
                        } => {
                            health.set(
                                gid,
                                CachedGlyphHealth::CodeDependentBlank {
                                    fallback: Some(unreadable),
                                },
                            );
                            any_unreadable |= unreadable;
                        }
                        // Exhausted probes remain uncached so they never mark
                        // later visible glyphs as confirmed readable.
                        zpdf_font::EmbeddedGlyphHealthOutcome::Exhausted => {}
                    },
                );
                health.cmap_budget = cmap_budget;
            } else {
                let mut cmap_budget = zpdf_font::CMAP_PROBE_BUDGET;
                font.classify_embedded_glyphs_with_codes(&to_probe, &mut cmap_budget, |_, out| {
                    match out {
                        zpdf_font::EmbeddedGlyphHealthOutcome::Unreadable => any_unreadable = true,
                        zpdf_font::EmbeddedGlyphHealthOutcome::CodeDependent { unreadable }
                        | zpdf_font::EmbeddedGlyphHealthOutcome::CodeDependentFallback {
                            unreadable,
                        } => {
                            any_unreadable |= unreadable;
                        }
                        zpdf_font::EmbeddedGlyphHealthOutcome::Readable
                        | zpdf_font::EmbeddedGlyphHealthOutcome::Exhausted => {}
                    }
                });
            }
        }
        any_unreadable
    }

    /// Record a lazy embedded-font fallback diagnostic when a visible/clipped
    /// show encounters an unreadable embedded glyph on a font whose font-wide
    /// probe initially accepted the embedded program as primary.
    fn ensure_lazy_embedded_fallback(&mut self, font_id: FontId) {
        if self.oc_suppressed() {
            return;
        }
        let Some(fc) = self.font_cache.as_deref_mut() else {
            return;
        };
        if fc.embedded_fallback(font_id).is_some() {
            return;
        }
        let first_failed_object = match fc.companion_kind(font_id) {
            Some(zpdf_font::CompanionKind::DescriptorSubstitute { font_file_id }) => font_file_id,
            _ => None,
        };
        let mut diag = zpdf_core::ProcessingDiagnostics::default();
        diag.record_embedded_font_fallback(first_failed_object);
        fc.record_embedded_fallback(font_id, diag);
    }

    /// Verify that a fallback text run contributing to the text sink decodes
    /// every shown code (even when all outlines draw or render mode 3 makes the
    /// run invisible). Load-time limit diagnostics are always merged once the
    /// show contributes to extraction, while `/ActualText` exemptions remain
    /// scoped to the per-code decodability check.
    fn check_embedded_fallback_text(&mut self, font_id: FontId, bytes: &[u8]) {
        if self.text_sink.is_none() || bytes.is_empty() || self.oc_suppressed() {
            return;
        }
        let Some(fallback_diag) = self.merge_embedded_fallback(font_id) else {
            return;
        };
        if self.active_actual_text.is_some()
            || self.current_struct_actual_text_covers()
            || self.reported_font_text_misses.contains(&font_id)
        {
            return;
        }
        if !self.fallback_text_decodable(font_id, bytes) {
            self.reported_font_text_misses.push(font_id);
            self.stats.diagnostics.decode_failures =
                self.stats.diagnostics.decode_failures.saturating_add(1);
            if let Some(id) = fallback_diag.first_failed_object {
                self.stats.diagnostics.first_failed_object.get_or_insert(id);
            }
        }
    }

    fn fallback_outlines_complete(
        &self,
        original_font_id: FontId,
        bytes: &[u8],
        glyphs: &[PositionedGlyph],
        resolved_runs: Option<&[(FontId, Vec<PositionedGlyph>)]>,
    ) -> bool {
        let Some(cache) = self.font_cache.as_deref() else {
            return false;
        };
        let Some(orig_font) = cache.get(original_font_id) else {
            return false;
        };
        let composite = matches!(orig_font.font_type, zpdf_font::PdfFontType::Type0CidType2);
        let cmap = orig_font.cid_cmap.as_ref();
        let mut offset = 0usize;
        let mut check_slice = |drawing_font_id: FontId, slice: &[PositionedGlyph]| -> bool {
            let Some(drawing_font) = cache.get(drawing_font_id) else {
                return false;
            };
            drawing_font.fallback_glyphs_complete(
                orig_font,
                slice.iter().map(|glyph| {
                    let (code, length, raw, end) = next_text_code(cmap, composite, bytes, offset)
                        .unwrap_or((0, 0, 0, bytes.len()));
                    offset = end;
                    (glyph.glyph_id, code, raw, length as u8)
                }),
            )
        };
        if let Some(runs) = resolved_runs {
            runs.iter()
                .all(|(run_font_id, run_glyphs)| check_slice(*run_font_id, run_glyphs))
        } else {
            check_slice(original_font_id, glyphs)
        }
    }

    fn fallback_text_decodable(&self, font_id: FontId, bytes: &[u8]) -> bool {
        let Some(orig_font) = self.font_cache.as_deref().and_then(|fc| fc.get(font_id)) else {
            return false;
        };
        let composite = matches!(orig_font.font_type, zpdf_font::PdfFontType::Type0CidType2);
        let cmap = orig_font.cid_cmap.as_ref();
        // Match extraction (`ToUnicodeMap::append_fixed_width`) when a Type0
        // fallback font carries no encoding CMap and declares a single fixed
        // `/ToUnicode` code width (e.g. 1-byte codes). Keep the encoding CMap
        // as the first choice whenever present.
        if composite && cmap.is_none() {
            if let Some(map) = orig_font.to_unicode.as_ref() {
                let width = match map.code_byte_lengths() {
                    &[width] => usize::from(width),
                    _ => 2,
                };
                return bytes.chunks(width).all(|chunk| {
                    let code = chunk.iter().fold(0u32, |v, &b| (v << 8) | b as u32);
                    map.lookup_with_length(code, chunk.len() as u8)
                        .or_else(|| map.lookup(code))
                        .is_some_and(|s| !s.is_empty())
                });
            }
        }
        let mut offset = 0usize;
        while offset < bytes.len() {
            let Some((code, length, raw, end)) = next_text_code(cmap, composite, bytes, offset)
            else {
                return false;
            };
            offset = end;
            if !orig_font.can_decode_code(code, length as u8, raw) {
                return false;
            }
        }
        true
    }

    /// Keep optional fallback and clipping state off the main decoding stack.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn paint_positioned_text(
        &mut self,
        bytes: &[u8],
        mut glyphs: Vec<PositionedGlyph>,
        combined: Matrix,
        font_size: f32,
        h_scale: f32,
        repair: bool,
        unreadable_embedded: bool,
    ) {
        // Only emit a glyph run when a font is actually active; with no font the
        // glyph IDs would be raw, unmappable codes aliased onto font 0.
        // Text render mode 3 (invisible, the OCR-layer case) and 7 (clip only)
        // paint nothing; stroke modes 1/2/5/6 use outline paths below.
        // A `/None` colorant space on the active paint (fill for modes 0/2/4/6,
        // stroke for 1/5) likewise produces no marks — suppress the visual run,
        // while the text-extraction sink below still records the glyphs.
        let no_marks = match self.current.render_mode {
            1 | 5 => cs_produces_no_marks(&self.current.stroke_cs),
            2 | 6 => {
                cs_produces_no_marks(&self.current.fill_cs)
                    && cs_produces_no_marks(&self.current.stroke_cs)
            }
            _ => cs_produces_no_marks(&self.current.fill_cs),
        };
        let is_type3 = self
            .current_font_id
            .and_then(|fid| self.font_cache.as_ref().and_then(|fc| fc.get(fid)))
            .is_some_and(|f| f.is_type3());
        let invisible = matches!(self.current.render_mode, 3 | 7) || (no_marks && !is_type3);
        let wants_clip = matches!(self.current.render_mode, 4..=7);
        let paints = !invisible && !self.oc_suppressed();
        if !invisible && self.oc_suppressed() {
            self.type3_oc_suppressed_mark = true;
        }
        // Gate drawing-font resolution on whether the show actually paints or
        // contributes a clip so disabled-OCG and `/None`-colorant shows never
        // permanently consume page font-cache budget.
        if !paints && !wants_clip {
            return;
        }

        let resolved_runs = self.current_font_id.and_then(|font_id| {
            self.resolve_text_glyphs(font_id, bytes, &mut glyphs, repair, unreadable_embedded)
        });

        // Text render modes 4–7 also *add the glyph outlines to the clipping
        // path* (applied at ET). Accumulate them in page space now — regardless
        // of whether the run is also painted (4=fill+clip, 5=stroke+clip,
        // 6=fill+stroke+clip, 7=clip only). Without this, a following opaque fill
        // or image meant to show only through the glyphs paints over everything.
        let text_pattern = match self.current.render_mode {
            1 | 5 => self.current.stroke_pattern.clone(),
            _ => self.current.fill_pattern.clone(),
        };
        if let Some(font_id) = self.current_font_id {
            if !glyphs.is_empty() || resolved_runs.is_some() {
                if unreadable_embedded {
                    self.ensure_lazy_embedded_fallback(font_id);
                }
                self.check_embedded_fallback_outlines(
                    font_id,
                    bytes,
                    &glyphs,
                    resolved_runs.as_deref(),
                );
            }
        }
        // No font at all leaves the shown codes without any outline source.
        let mut outlines_unavailable = self.current_font_id.is_none();
        // Per resolved run: true when its font has no outlines to clip with.
        let mut outline_less_runs: Vec<bool> = Vec::new();
        let glyph_clip = if wants_clip || text_pattern.is_some() {
            let mut union: Option<(Path, Rect)> = None;
            let mut append = |font_id, glyphs: &[PositionedGlyph]| {
                let font = self
                    .font_cache
                    .as_ref()
                    .and_then(|cache| cache.get(font_id))
                    .filter(|font| glyph_outlines_available(font));
                let Some(font) = font else {
                    outlines_unavailable = true;
                    return false;
                };
                let mut failed = false;
                if let Some((path, bounds)) =
                    build_glyph_clip_path(glyphs, &combined, font, font_size, h_scale, &mut failed)
                {
                    match &mut union {
                        Some((all, total)) => {
                            all.extend_from_path(&path);
                            *total = Rect::new(
                                total.x0.min(bounds.x0),
                                total.y0.min(bounds.y0),
                                total.x1.max(bounds.x1),
                                total.y1.max(bounds.y1),
                            );
                        }
                        None => union = Some((path, bounds)),
                    }
                }
                // An unreadable glyph is not blank: its clip area is unknown.
                outlines_unavailable |= failed;
                true
            };
            if let Some(runs) = &resolved_runs {
                outline_less_runs = runs
                    .iter()
                    .map(|(font_id, glyphs)| !append(*font_id, glyphs))
                    .collect();
            } else if let Some(font_id) = self.current_font_id {
                append(font_id, &glyphs);
            }
            union
        } else {
            None
        };
        if wants_clip && (!glyphs.is_empty() || resolved_runs.is_some()) {
            self.text_clip_requested = true;
            self.text_clip_unavailable |= outlines_unavailable;
            if let Some((path, _)) = &glyph_clip {
                self.text_clip_accum
                    .get_or_insert_with(Path::new)
                    .extend_from_path(path);
            }
        }
        if let Some(font_id) = self.current_font_id {
            if (!glyphs.is_empty() || resolved_runs.is_some()) && paints {
                if matches!(self.current.render_mode, 1 | 2 | 5 | 6) {
                    if let Some(runs) = resolved_runs {
                        for (id, glyphs) in runs {
                            self.paint_stroked_text(id, glyphs, combined, font_size, h_scale);
                        }
                    } else {
                        self.paint_stroked_text(font_id, glyphs, combined, font_size, h_scale);
                    }
                    return;
                }
                // A Pattern colour space on the active paint (fill for modes
                // 0/2/4/6, stroke for 1/5) clips the pattern to the glyph
                // outlines instead of painting a solid run, or passes
                // `Paint::Tiling` directly on `DrawGlyphRun` for outline-less
                // Type 3 fonts.
                let painted = match (text_pattern, glyph_clip) {
                    (Some(PatternPaint::Shading(def)), Some((clip_path, bounds))) => {
                        self.display_list.push(RenderCommand::PushClip {
                            path: clip_path,
                            rule: FillRule::NonZero,
                        });
                        self.emit_shading_image(&def, bounds);
                        self.display_list.push(RenderCommand::PopClip);
                        true
                    }
                    (Some(PatternPaint::Tiling { def, base_ctm }), Some((clip_path, bounds))) => {
                        if self.oc_suppressed() {
                            self.type3_oc_suppressed_mark = true;
                            true
                        } else if let Some(tiling) =
                            self.build_tiling_pattern_paint(&def, base_ctm, self.current.fill_color)
                        {
                            // Pad to integer coordinates outside `bounds` so the
                            // clipped fill has 100% coverage on every anti-aliased
                            // glyph boundary pixel (no double-AA attenuation).
                            let mut rect_path = Path::new();
                            rect_path.rect(Rect::new(
                                bounds.x0.floor() - 1.0,
                                bounds.y0.floor() - 1.0,
                                bounds.x1.ceil() + 1.0,
                                bounds.y1.ceil() + 1.0,
                            ));
                            self.display_list.push(RenderCommand::PushClip {
                                path: clip_path,
                                rule: FillRule::NonZero,
                            });
                            let cmd = RenderCommand::FillPath {
                                path: rect_path,
                                rule: FillRule::NonZero,
                                paint: Paint::Tiling(tiling),
                                alpha: self.current.fill_alpha,
                                overprint: None,
                            };
                            self.emit_painted(cmd);
                            self.display_list.push(RenderCommand::PopClip);
                            true
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if !painted {
                    if let Some(runs) = resolved_runs {
                        for (id, glyphs) in runs {
                            self.emit_solid_glyph_run(id, font_size, glyphs, combined, h_scale);
                        }
                    } else {
                        self.emit_solid_glyph_run(font_id, font_size, glyphs, combined, h_scale);
                    }
                } else if let Some(runs) = resolved_runs {
                    // Runs without outlines (Type 3 CharProcs) lie outside the
                    // outline clip; expand them via `emit_solid_glyph_run` ->
                    // `paint_type3_glyph_run` with the active fill pattern intact.
                    for ((id, glyphs), _) in runs
                        .into_iter()
                        .zip(outline_less_runs)
                        .filter(|(_, outline_less)| *outline_less)
                    {
                        self.emit_solid_glyph_run(id, font_size, glyphs, combined, h_scale);
                    }
                }
            }
        }
    }

    /// ISO 32000-1 9.3.6: fill then stroke each glyph, using user-space line
    /// parameters (not text-matrix-scaled widths), separate paints and alpha.
    fn paint_stroked_text(
        &mut self,
        font_id: FontId,
        glyphs: Vec<PositionedGlyph>,
        transform: Matrix,
        font_size: f32,
        h_scale: f32,
    ) {
        let type3 = self
            .font_cache
            .as_ref()
            .and_then(|c| c.get(font_id))
            .is_some_and(|font| font.is_type3());
        if type3 {
            self.emit_solid_glyph_run(font_id, font_size, glyphs, transform, h_scale);
            return;
        }
        let fill = matches!(self.current.render_mode, 2 | 6);
        for glyph in glyphs {
            if self.over_budget() {
                break;
            }
            let outline = self
                .font_cache
                .as_ref()
                .and_then(|c| c.get(font_id))
                .and_then(|font| {
                    build_glyph_clip_path(
                        std::slice::from_ref(&glyph),
                        &transform,
                        font,
                        font_size,
                        h_scale,
                        &mut false,
                    )
                });
            if let Some((path, _)) = outline {
                if fill {
                    self.fill_page_path(path.clone(), FillRule::NonZero);
                }
                if self.should_expand_anisotropic_stroke() {
                    if let Some(inv_ctm) = self.current.ctm.inverse() {
                        let user_path = path.map_points(|p| p.transform(&inv_ctm));
                        let remaining = self
                            .max_anisotropic_stroke_points
                            .saturating_sub(self.anisotropic_stroke_points);
                        match self.expand_anisotropic_stroke(&user_path, remaining) {
                            AnisotropicStroke::Outline(stroked) => {
                                self.anisotropic_stroke_points = self
                                    .anisotropic_stroke_points
                                    .saturating_add(stroked.points().len());
                                self.emit_stroke_as_filled_outline(stroked);
                                continue;
                            }
                            AnisotropicStroke::Empty => continue,
                            AnisotropicStroke::Fallback => {}
                        }
                    }
                }
                self.stroke_page_path(path);
            } else if fill {
                self.emit_solid_glyph_run(font_id, font_size, vec![glyph], transform, h_scale);
            }
        }
    }

    /// Keep the original positions/advances and replace only drawing identities.
    /// The absent/empty-provider path allocates no resolution state or vectors.
    /// `repair` marks a show with broken-subset blanks (see `font_repair`):
    /// those draw from a validated page font first, then from the provider.
    fn resolve_text_glyphs(
        &mut self,
        font_id: FontId,
        bytes: &[u8],
        glyphs: &mut Vec<PositionedGlyph>,
        repair: bool,
        unreadable_embedded: bool,
    ) -> Option<Vec<(FontId, Vec<PositionedGlyph>)>> {
        if self.current.render_mode == 3 || glyphs.is_empty() {
            return None;
        }
        let max_bytes = self.max_font_cache_bytes;
        let file = self.file;
        let shared_doc = self.shared_font_doc;
        let scope = crate::font_repair::RepairScope {
            page: self.resources,
            forms: &self.form_font_overrides,
        };
        let repairs = &mut self.font_repair;
        let mut fallback_cmap_budget = zpdf_font::CMAP_PROBE_BUDGET;
        let mut glyph_health = self
            .embedded_glyph_health
            .iter_mut()
            .find(|(id, _)| *id == font_id)
            .map(|(_, health)| health);
        let cache = self.font_cache.as_deref_mut()?;
        let provider_arc = cache.provider().cloned();
        let provider = provider_arc
            .as_ref()
            .is_some_and(|provider| provider.generation() != 0);
        let has_companion = cache.has_embedded_companion_for_budget(font_id, max_bytes)
            || (unreadable_embedded
                && cache.has_descriptor_substitute_for_budget(font_id, max_bytes));
        if !provider && !repair && !has_companion {
            return None;
        }
        let composite = matches!(
            cache.get(font_id)?.font_type,
            zpdf_font::PdfFontType::Type0CidType2
        );
        let cmap = cache.get(font_id)?.cid_cmap.as_ref();
        let mut check_offset = 0usize;
        if !repair && !unreadable_embedded {
            let all_covered =
                if has_companion {
                    cache.get(font_id).is_some_and(|font| {
                        font.fallback_glyphs_complete(
                            font,
                            glyphs.iter().map(|glyph| {
                                let (code, length, raw, end) =
                                    next_text_code(cmap, composite, bytes, check_offset)
                                        .unwrap_or((0, 0, 0, bytes.len()));
                                check_offset = end;
                                (glyph.glyph_id, code, raw, length as u8)
                            }),
                        )
                    })
                } else {
                    cache.original_glyphs_usable(
                        font_id,
                        glyphs.iter().map(|glyph| {
                            let (code, length, raw, end) = next_text_code(
                                cmap,
                                composite,
                                bytes,
                                check_offset,
                            )
                            .unwrap_or((0, 0, 0, bytes.len()));
                            check_offset = end;
                            (glyph.glyph_id, code, raw, length as u8)
                        }),
                    )
                };
            if all_covered {
                return None;
            }
        }
        let mut ids: Option<Vec<FontId>> = None;
        let mut offset = 0;
        let mut previous = None;
        let mut run_loaded_companion: Option<std::sync::Arc<zpdf_font::LoadedFont>> = None;
        for (index, glyph) in glyphs.iter_mut().enumerate() {
            let cmap = cache.get(font_id)?.cid_cmap.as_ref();
            let (code, length, raw, end) = next_text_code(cmap, composite, bytes, offset)?;
            offset = end;
            let original = zpdf_font::DrawingGlyph {
                font_id,
                glyph_id: glyph.glyph_id,
            };
            let key = (code, raw, length, original.glyph_id);
            // Adjacent repeats share a resolution within this positioned text
            // operation. Later operations still observe provider generation.
            let suspicious = repair
                && cache.get(font_id).is_some_and(|font| {
                    repairs.is_suspicious(font_id, font, original.glyph_id, code, raw, length as u8)
                });
            let drawing = match previous {
                Some((previous_key, drawing)) if previous_key == key => drawing,
                _ => {
                    let sibling = if suspicious {
                        repairs.sibling_glyph(cache, &scope, font_id, code, length as u8, raw)
                    } else {
                        None
                    };
                    let drawing = match sibling {
                        Some(sibling) => sibling,
                        None => {
                            let embedded = if has_companion {
                                let (original_unreadable, cmap_budget) =
                                    if !unreadable_embedded || original.glyph_id == 0 {
                                        (Some(false), None)
                                    } else if let Some(health) = glyph_health.as_deref_mut() {
                                        let known = match health.get(original.glyph_id) {
                                            Some(CachedGlyphHealth::Readable) => Some(false),
                                            Some(CachedGlyphHealth::Unreadable) => Some(true),
                                            Some(CachedGlyphHealth::CodeDependentBlank {
                                                fallback,
                                            }) => cache
                                                .get(font_id)
                                                .and_then(|f| {
                                                    f.code_dependent_blank_is_unreadable(
                                                        code,
                                                        raw,
                                                        length as u8,
                                                    )
                                                })
                                                .or(fallback),
                                            None => None,
                                        };
                                        (known, Some(&mut health.cmap_budget))
                                    } else {
                                        (None, Some(&mut fallback_cmap_budget))
                                    };
                                cache.resolve_embedded_companion_glyph_in_run(
                                    original,
                                    code,
                                    raw,
                                    length as u8,
                                    max_bytes,
                                    original_unreadable,
                                    cmap_budget,
                                    Some(&mut run_loaded_companion),
                                    |source, remaining| {
                                        let Some(file) = file else {
                                            return zpdf_font::DeferredCompanionLoad::Unavailable;
                                        };
                                        let loaded = match source {
                                            zpdf_font::CompanionSource::Object(font_ref) => {
                                                shared_doc
                                                    .filter(|doc| {
                                                        std::ptr::eq(doc.file(), file)
                                                            && match (
                                                                doc.font_provider(),
                                                                provider_arc.as_ref(),
                                                            ) {
                                                                (None, None) => true,
                                                                (Some(a), Some(b)) => {
                                                                    std::sync::Arc::ptr_eq(a, b)
                                                                }
                                                                _ => false,
                                                            }
                                                    })
                                                    .and_then(|doc| {
                                                        doc.cached_shared_font_companion(*font_ref)
                                                    })
                                                    .or_else(|| {
                                                        zpdf_document::font_loader::load_embedded_companion_ref(
                                                            file, *font_ref, remaining,
                                                        )
                                                        .map(std::sync::Arc::new)
                                                    })
                                            }
                                            zpdf_font::CompanionSource::FormInlineFont {
                                                form_id,
                                                font_name,
                                            } => {
                                                zpdf_document::font_loader::load_embedded_companion_form_inline(
                                                    file, *form_id, font_name, remaining,
                                                )
                                                .map(std::sync::Arc::new)
                                            }
                                            zpdf_font::CompanionSource::ResourcesInlineFont {
                                                resources_id,
                                                font_name,
                                            } => {
                                                zpdf_document::font_loader::load_embedded_companion_resources_inline(
                                                    file, *resources_id, font_name, remaining,
                                                )
                                                .map(std::sync::Arc::new)
                                            }
                                            zpdf_font::CompanionSource::None => None,
                                        };
                                        match loaded {
                                            Some(comp) => {
                                                zpdf_font::DeferredCompanionLoad::Loaded(comp)
                                            }
                                            None => {
                                                zpdf_font::DeferredCompanionLoad::FailedForBudget
                                            }
                                        }
                                    },
                                )
                            } else {
                                None
                            };
                            match embedded {
                                Some(embedded) => embedded,
                                None if provider => cache.resolve_drawing_glyph(
                                    original,
                                    code,
                                    raw,
                                    length as u8,
                                    max_bytes,
                                ),
                                None => original,
                            }
                        }
                    };
                    previous = Some((key, drawing));
                    drawing
                }
            };
            if drawing != original && ids.is_none() {
                ids = Some(vec![font_id; index]);
            }
            glyph.glyph_id = drawing.glyph_id;
            if let Some(ids) = &mut ids {
                ids.push(drawing.font_id);
            }
        }
        let ids = ids?;
        let mut runs: Vec<(FontId, Vec<PositionedGlyph>)> = Vec::new();
        for (id, glyph) in ids.into_iter().zip(std::mem::take(glyphs)) {
            if let Some((_, current)) = runs.last_mut().filter(|(previous, _)| *previous == id) {
                current.push(glyph);
            } else {
                runs.push((id, vec![glyph]));
            }
        }
        Some(runs)
    }

    /// At `ET`, fold any text-clip accumulated during the block (render modes
    /// 4–7) into the clip stack. The glyph outlines are already in page space,
    /// so this mirrors a `W n` clip: push a NonZero `PushClip` and bump
    /// `clip_depth` so the enclosing `Q` pops it. Without this, a subsequent
    /// opaque fill or image meant to show only through the glyph shapes paints
    /// over the whole surrounding clip region.
    ///
    /// Glyphs whose outlines are known but empty (spaces) contribute an empty
    /// clipping region: the empty `PushClip` hides a later page-covering fill
    /// in every backend. When the accumulated path is empty only because some
    /// glyphs had no outline source (Type3 CharProcs, no loaded font program),
    /// the real clip is unknown; skip it rather than hide all later painting,
    /// as pdf.js does for Type3 text.
    fn apply_text_clip(&mut self) {
        if !self.text_clip_requested {
            return;
        }
        self.text_clip_requested = false;
        let unavailable = std::mem::take(&mut self.text_clip_unavailable);
        let path = self.text_clip_accum.take().unwrap_or_default();
        if path.is_empty() && unavailable {
            tracing::debug!("text clip skipped: clipped glyphs have no outline source");
            return;
        }
        self.intersect_clip_bounds(Self::path_bounds(&path));
        self.intersect_clip_elision_bounds(self.clip_path_enforced_device_bounds(&path));
        self.display_list.push(RenderCommand::PushClip {
            path,
            rule: FillRule::NonZero,
        });
        self.current.clip_depth += 1;
    }

    fn adjust_text_position(&mut self, amount: f64) {
        // TJ displacement: amount is in thousandths of a unit of text space.
        // Per PDF 9.4.4 it moves tx (scaled by Th) in horizontal mode but ty
        // (no Th factor) in vertical writing mode.
        let vertical = self
            .current_font_id
            .and_then(|fid| self.font_cache.as_ref().and_then(|fc| fc.get(fid)))
            .and_then(|f| f.cid_cmap.as_ref())
            .map(|c| c.wmode == 1)
            .unwrap_or(false);
        let advance = if vertical {
            let displacement = amount / 1000.0 * self.current.font_size as f64;
            Matrix::translate(0.0, displacement)
        } else {
            let displacement = amount / 1000.0
                * self.current.font_size as f64
                * (self.current.h_scaling as f64 / 100.0);
            Matrix::translate(displacement, 0.0)
        };
        self.text_matrix = self.text_matrix.concat(&advance);
    }
}

/// Resolve any indirect-reference values among an image XObject's scalar
/// metadata keys so the image decoder sees concrete values — notably an indirect
/// `/Decode` must be resolved or an `/ImageMask` stencil would paint with the
/// wrong polarity. `/SMask` and `/Mask` are intentionally left as references
/// (handled separately, and they are streams we don't want to inline here).
fn resolve_image_metadata(file: &PdfFile, dict: &zpdf_core::PdfDict) -> zpdf_core::PdfDict {
    use zpdf_core::{PdfName, PdfObject};
    let mut out = dict.clone();
    for key in [
        "Width",
        "Height",
        "BitsPerComponent",
        "ImageMask",
        "Decode",
        "ColorSpace",
        // zpdf-image sniffs /Filter (DCT/JPX detection); an indirect filter
        // value would make decoded JPEG pixels look like raw samples.
        "Filter",
        // Inlines a colour-key /Mask array hiding behind a ref; a stencil
        // /Mask stream ref stays a ref in the original dict and is handled by
        // fold_stencil_mask.
        "Mask",
    ] {
        let r = match out.get(key) {
            Some(PdfObject::Ref(r)) => Some(*r),
            _ => None,
        };
        if let Some(r) = r {
            if let Ok(v) = file.resolve(r) {
                out.insert(PdfName::new(key), v);
            }
        }
        // A /Filter array may itself hold indirect name elements.
        if key == "Filter" {
            if let Some(PdfObject::Array(arr)) = out.get(key) {
                let resolved: Vec<PdfObject> = arr
                    .iter()
                    .map(|o| match o {
                        PdfObject::Ref(r) => file.resolve(*r).unwrap_or(PdfObject::Null),
                        other => other.clone(),
                    })
                    .collect();
                out.insert(PdfName::new(key), PdfObject::Array(resolved));
            }
        }
    }
    out
}

/// Split the character code at `bytes[offset..]` as `show_text` did: through
/// the encoding CMap for composite fonts (two-byte codes without one), one
/// byte for simple fonts. Returns `(code, length, raw source value, end)`.
pub(crate) fn next_text_code(
    cmap: Option<&zpdf_font::cmap::CidCMap>,
    composite: bool,
    bytes: &[u8],
    offset: usize,
) -> Option<(u32, usize, u32, usize)> {
    let fold = |codes: &[u8]| {
        codes
            .iter()
            .fold(0, |value, &byte| (value << 8) | byte as u32)
    };
    let (code, length) = match (composite, cmap) {
        (true, Some(cmap)) => cmap.next_code(bytes.get(offset..)?),
        (true, None) => {
            let length = (bytes.len().checked_sub(offset)?).min(2);
            (fold(&bytes[offset..offset + length]), length)
        }
        (false, _) => (*bytes.get(offset)? as u32, 1),
    };
    let end = (offset + length.max(1)).min(bytes.len());
    Some((code, length, fold(&bytes[offset..end]), end))
}

/// Vertical extent of a selection cell in em, as `(descent, ascent)`.
///
/// A font's line metrics cannot be used unchecked: math and Nastaliq faces
/// declare several em, and some store the descender as a positive number.
/// The cell stays within 1 em above and half an em below the baseline, so it
/// cannot cover the neighbouring lines. Missing or non-finite metrics give
/// the conventional 0.8/−0.2 box.
fn selection_cell_extent(font: Option<&zpdf_font::LoadedFont>) -> (f64, f64) {
    const DEFAULT: (f64, f64) = (-0.2, 0.8);
    let Some(font) = font.filter(|f| f.units_per_em.is_finite() && f.units_per_em > 0.0) else {
        return DEFAULT;
    };
    let ascent = font.ascent / font.units_per_em;
    let descent = font.descent / font.units_per_em;
    if !(ascent.is_finite() && descent.is_finite() && ascent > 0.0) {
        return DEFAULT;
    }
    (-descent.abs().min(0.5), ascent.clamp(0.5, 1.0))
}

/// Whether `build_glyph_clip_path` can read outlines from this font at all.
/// Type3 glyphs are content streams and a font without a program has no
/// outlines; for other fonts a glyph without an outline is genuinely blank.
fn glyph_outlines_available(font: &zpdf_font::LoadedFont) -> bool {
    font.has_font_data() && !font.is_type3() && font.units_per_em > 0.0
}

/// Build a combined glyph-outline clip path (in PAGE space) for a positioned
/// glyph run, plus its bounding box. Used to clip a pattern/shading paint to
/// text. Returns `None` for fonts with no usable outlines (bitmap/Type3) or an
/// all-blank run. Quadratic segments are promoted to cubics for the display
/// list `Path` (which carries cubics only). Sets `failed` when a glyph has no
/// outline without being a well-formed blank glyph.
fn build_glyph_clip_path(
    glyphs: &[PositionedGlyph],
    tm: &Matrix,
    font: &zpdf_font::LoadedFont,
    font_size: f32,
    h_scale: f32,
    failed: &mut bool,
) -> Option<(Path, Rect)> {
    if !glyph_outlines_available(font) {
        return None;
    }
    let upem = font.units_per_em as f32;

    // Glyph-space point (font units, + the glyph's text-space offset) → page.
    // The shape x carries the horizontal-scaling factor (Th); the offset already
    // does (it's an accumulated advance), so Th multiplies only the outline term.
    let to_page = |gx: f64, gy: f64, ox: f32, oy: f32| -> Point {
        let tx = (gx as f32 / upem * font_size * h_scale + ox) as f64;
        let ty = (gy as f32 / upem * font_size + oy) as f64;
        Point::new(tm.a * tx + tm.c * ty + tm.e, tm.b * tx + tm.d * ty + tm.f)
    };

    let mut all = Path::new();
    // One glyph at a time, oriented before it joins the run's clip path.
    let mut glyph_path = Path::new();
    let mut bb = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let track = |p: Point, bb: &mut (f64, f64, f64, f64)| {
        bb.0 = bb.0.min(p.x);
        bb.1 = bb.1.min(p.y);
        bb.2 = bb.2.max(p.x);
        bb.3 = bb.3.max(p.y);
    };
    let mut any = false;

    for glyph in glyphs {
        let outline = match font.glyph_outline(glyph.glyph_id) {
            Some(o) => o,
            None => {
                *failed |= !font.is_blank_glyph(glyph.glyph_id);
                continue;
            }
        };
        let (ox, oy) = (glyph.x, glyph.y);
        let mut cur = Point::new(0.0, 0.0);
        let out = &mut glyph_path;
        out.clear();
        for cmd in &outline.commands {
            match *cmd {
                zpdf_font::OutlineCommand::MoveTo(x, y) => {
                    let p = to_page(x, y, ox, oy);
                    out.move_to(p);
                    track(p, &mut bb);
                    cur = p;
                    any = true;
                }
                zpdf_font::OutlineCommand::LineTo(x, y) => {
                    let p = to_page(x, y, ox, oy);
                    out.line_to(p);
                    track(p, &mut bb);
                    cur = p;
                }
                zpdf_font::OutlineCommand::QuadTo(cx, cy, x, y) => {
                    let c = to_page(cx, cy, ox, oy);
                    let e = to_page(x, y, ox, oy);
                    // Quadratic → cubic: control points at 2/3 toward the quad ctrl.
                    let c1 = Point::new(
                        cur.x + 2.0 / 3.0 * (c.x - cur.x),
                        cur.y + 2.0 / 3.0 * (c.y - cur.y),
                    );
                    let c2 =
                        Point::new(e.x + 2.0 / 3.0 * (c.x - e.x), e.y + 2.0 / 3.0 * (c.y - e.y));
                    out.curve_to(c1, c2, e);
                    track(c1, &mut bb);
                    track(c2, &mut bb);
                    track(e, &mut bb);
                    cur = e;
                }
                zpdf_font::OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                    let c1 = to_page(x1, y1, ox, oy);
                    let c2 = to_page(x2, y2, ox, oy);
                    let e = to_page(x, y, ox, oy);
                    out.curve_to(c1, c2, e);
                    track(c1, &mut bb);
                    track(c2, &mut bb);
                    track(e, &mut bb);
                    cur = e;
                }
                zpdf_font::OutlineCommand::Close => out.close(),
            }
        }
        orient_glyph_contours(&mut glyph_path);
        all.extend_from_path(&glyph_path);
    }

    if !(any && bb.0.is_finite() && bb.2 > bb.0 && bb.3 > bb.1) {
        return None;
    }
    Some((all, Rect::new(bb.0, bb.1, bb.2, bb.3)))
}

/// Orient the contours of one glyph's `path` counter-clockwise in page
/// space. TrueType outlines run clockwise and CFF ones the other way; a
/// nonzero fill of glyphs from both would cancel their overlap into a hole
/// instead of taking the union. Reversing every contour of the glyph together
/// keeps its own counters intact.
fn orient_glyph_contours(path: &mut Path) {
    let cross = |a: Point, b: Point| a.x * b.y - b.x * a.y;
    // Shoelace over the control polygon; its sign matches the outline's.
    let mut area = 0.0;
    let (mut first, mut prev) = (None::<Point>, None::<Point>);
    for element in &*path {
        match element {
            PathElement::MoveTo(p) => {
                if let (Some(f), Some(q)) = (first, prev) {
                    area += cross(q, f);
                }
                (first, prev) = (Some(p), Some(p));
            }
            PathElement::LineTo(p) => {
                let Some(q) = prev else { return };
                area += cross(q, p);
                prev = Some(p);
            }
            PathElement::CurveTo(c1, c2, p) => {
                let Some(q) = prev else { return };
                area += cross(q, c1) + cross(c1, c2) + cross(c2, p);
                prev = Some(p);
            }
            PathElement::Close => {}
        }
    }
    if let (Some(f), Some(q)) = (first, prev) {
        area += cross(q, f);
    }
    if area.is_nan() || area >= 0.0 {
        return;
    }

    fn flush(contour: &mut Vec<PathElement>, out: &mut Path) {
        let Some(PathElement::MoveTo(origin)) = contour.first().copied() else {
            out.extend(contour.drain(..));
            return;
        };
        let closed = matches!(contour.last(), Some(PathElement::Close));
        let segments = &contour[1..contour.len() - closed as usize];
        let mut starts = Vec::with_capacity(segments.len());
        let mut at = origin;
        for segment in segments {
            starts.push(at);
            if let PathElement::LineTo(p) | PathElement::CurveTo(_, _, p) = *segment {
                at = p;
            }
        }
        out.move_to(at);
        for (segment, from) in segments.iter().zip(starts).rev() {
            out.push(match *segment {
                PathElement::CurveTo(c1, c2, _) => PathElement::CurveTo(c2, c1, from),
                _ => PathElement::LineTo(from),
            });
        }
        if closed {
            out.close();
        }
        contour.clear();
    }
    let glyph: Vec<PathElement> = path.iter().collect();
    path.clear();
    let mut contour: Vec<PathElement> = Vec::new();
    for element in glyph {
        if matches!(element, PathElement::MoveTo(_)) && !contour.is_empty() {
            flush(&mut contour, path);
        }
        contour.push(element);
    }
    flush(&mut contour, path);
}

/// Wrap a generated form-field appearance as a form-XObject stream so the
/// annotation painter can replay it through `do_form_xobject` exactly like a
/// real `/AP /N`. The stream carries no `/Filter`, so the form emitter's direct
/// `decode_stream` fallback returns the content bytes verbatim.
fn synthetic_form_stream(gen: &zpdf_document::forms::GeneratedAppearance) -> zpdf_core::PdfStream {
    use zpdf_core::{PdfName, PdfObject};
    let num = |v: f64| PdfObject::Real(v);
    let mut dict = zpdf_core::PdfDict::new();
    dict.insert(
        PdfName::new("Type"),
        PdfObject::Name(PdfName::new("XObject")),
    );
    dict.insert(
        PdfName::new("Subtype"),
        PdfObject::Name(PdfName::new("Form")),
    );
    dict.insert(
        PdfName::new("BBox"),
        PdfObject::Array(vec![
            num(gen.bbox.x0),
            num(gen.bbox.y0),
            num(gen.bbox.x1),
            num(gen.bbox.y1),
        ]),
    );
    let m = &gen.matrix;
    dict.insert(
        PdfName::new("Matrix"),
        PdfObject::Array(vec![
            num(m.a),
            num(m.b),
            num(m.c),
            num(m.d),
            num(m.e),
            num(m.f),
        ]),
    );
    dict.insert(
        PdfName::new("Resources"),
        PdfObject::Dict(gen.resources.clone()),
    );
    zpdf_core::PdfStream::new(dict, gen.content.clone())
}

/// Decode an in-memory stream with the owning file's active limits.
/// The owning file is optional for synthetic standalone content.
fn decode_stream_for_file(
    file: Option<&PdfFile>,
    data: &[u8],
    dict: &PdfDict,
) -> zpdf_core::Result<Vec<u8>> {
    match file {
        Some(file) => zpdf_parser::filters::decode_stream_with_limits(data, dict, file.limits()),
        None => zpdf_parser::filters::decode_stream(data, dict),
    }
}

/// [`ContentInterpreter::shading_pattern_cache`] key: the pattern object and
/// the bits of the `base_ctm` its shading was built under.
type ShadingPatternKey = (ObjectId, [u64; 6]);
/// A built shading pattern and its average colour (the stroke fallback).
type ShadingPatternPaint = (std::sync::Arc<crate::shading::ShadingDef>, Color);

fn matrix_bits(m: &Matrix) -> [u64; 6] {
    let norm = |v: f64| {
        if v == 0.0 {
            0.0f64.to_bits()
        } else {
            v.to_bits()
        }
    };
    [m.a, m.b, m.c, m.d, m.e, m.f].map(norm)
}

/// `resolve_pattern`'s result for a (possibly unusable) shading pattern.
fn shading_pattern_paint(built: Option<&ShadingPatternPaint>) -> (Option<PatternPaint>, Color) {
    match built {
        Some((def, approx)) => (Some(PatternPaint::Shading(def.clone())), *approx),
        None => (None, Color::gray(0.5)),
    }
}

/// Parse the font dictionary `font_ref` into `fc` under `name`. Through `doc`'s
/// shared cache (parsed once per document) when `doc` serves this file and
/// the cache's font provider; otherwise parsed afresh.
fn load_font_ref(
    doc: Option<&zpdf_document::PdfDocument>,
    file: &PdfFile,
    fc: &mut FontCache,
    font_ref: ObjectId,
    name: &str,
    max_bytes: u64,
) -> Result<Option<zpdf_core::ProcessingDiagnostics>, zpdf_core::Error> {
    let shared = doc.filter(|doc| {
        std::ptr::eq(doc.file(), file)
            && match (doc.font_provider(), fc.provider()) {
                (None, None) => true,
                (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
                _ => false,
            }
    });
    let (font, diagnostics, companion, descriptor_spec) = match shared {
        Some(doc) => doc.shared_font_with_fallback(font_ref)?,
        None => {
            let (font, diagnostics, companion, descriptor_spec) =
                zpdf_document::font_loader::load_single_font_with_fallback(
                    file,
                    font_ref,
                    fc.provider().map(|provider| provider.as_ref()),
                )?;
            (
                std::sync::Arc::new(font),
                diagnostics,
                companion.map(std::sync::Arc::new),
                descriptor_spec,
            )
        }
    };
    match fc.admit_with_diagnostics(
        name.to_string(),
        font,
        diagnostics,
        Some(font_ref),
        max_bytes,
    ) {
        Ok(fid) => {
            if fc.get(fid).is_some_and(|f| f.has_font_data()) {
                if let Some(spec) = descriptor_spec {
                    fc.record_descriptor_substitute(fid, spec, max_bytes);
                } else if companion.is_some() {
                    let _ = fc.record_companion(
                        fid,
                        zpdf_font::CompanionKind::EmbeddedOriginal,
                        zpdf_font::CompanionSource::Object(font_ref),
                        companion,
                        max_bytes,
                    );
                }
            }
            Ok(None)
        }
        Err(unadmitted) => Ok(Some(unadmitted)),
    }
}

/// Content key for an inline font dictionary: the same dictionary always
/// maps to the same font-cache slot, different ones (almost surely) do not.
fn inline_font_key(font_dict: &zpdf_core::PdfDict) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("{font_dict:?}").hash(&mut hasher);
    hasher.finish()
}

/// Build the tiling pattern `obj` (object `pat_id`, resource `name`), with its
/// cell content decoded. `None` when the pattern is unusable: not a stream,
/// undecodable, or with a degenerate /BBox or steps.
fn build_tiling_pattern(
    file: &PdfFile,
    pat_id: ObjectId,
    obj: &PdfObject,
    matrix: Matrix,
    name: &str,
) -> Option<std::sync::Arc<TilingPatternDef>> {
    let PdfObject::Stream(stream) = obj else {
        return None;
    };
    let dict = &stream.dict;
    let content = match file
        .resolve_stream_data(pat_id)
        .or_else(|_| decode_stream_for_file(Some(file), &stream.data, dict))
    {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("tiling pattern {name}: undecodable stream - {e}");
            return None;
        }
    };
    let get4 = |key: &str| {
        dict.get_array(key).ok().and_then(|arr| {
            let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
            (v.len() == 4).then(|| {
                Rect::new(
                    v[0].min(v[2]),
                    v[1].min(v[3]),
                    v[0].max(v[2]),
                    v[1].max(v[3]),
                )
            })
        })
    };
    let Some(bbox) = get4("BBox").filter(|b| b.width() > 0.0 && b.height() > 0.0) else {
        tracing::debug!("tiling pattern {name}: missing/degenerate BBox");
        return None;
    };
    // Steps default to the cell size; zero/non-finite steps are degenerate.
    let x_step = dict.get_f64("XStep").unwrap_or(bbox.width());
    let y_step = dict.get_f64("YStep").unwrap_or(bbox.height());
    if !x_step.is_finite() || !y_step.is_finite() || x_step == 0.0 || y_step == 0.0 {
        tracing::debug!("tiling pattern {name}: degenerate steps");
        return None;
    }
    let paint_type = dict.get_i64("PaintType").unwrap_or(1);
    let tiling_type = dict
        .get_i64("TilingType")
        .ok()
        .and_then(|v| u8::try_from(v).ok())
        .filter(|&v| (1..=3).contains(&v))
        .unwrap_or(1);
    Some(std::sync::Arc::new(TilingPatternDef {
        pat_id,
        content,
        dict: dict.clone(),
        bbox,
        x_step,
        y_step,
        matrix,
        paint_type,
        tiling_type,
    }))
}

/// Build an ICC transform from an inline (synthetic) profile stream.
fn build_inline_icc_transform(
    file: Option<&PdfFile>,
    s: &zpdf_core::PdfStream,
    intent: zpdf_color::RenderIntent,
) -> Option<std::sync::Arc<zpdf_color::IccTransform>> {
    let data = decode_stream_for_file(file, &s.data, &s.dict).ok()?;
    match zpdf_color::IccTransform::from_profile_bytes(&data, intent) {
        Ok(t) => Some(std::sync::Arc::new(t)),
        Err(e) => {
            tracing::warn!("inline ICC profile rejected: {e}; using /N fallback");
            None
        }
    }
}

/// Resolve a PDF `/ColorSpace` object into the pre-digested form consumed by
/// zpdf-image (which has no `PdfFile` access): chases indirect refs, ICCBased
/// profiles (compiled into transforms through `icc`, falling back to `/N`
/// 1/3/4 → gray/RGB/CMYK), Indexed palettes (string or stream; an ICC base
/// is baked into the palette), Lab parameters and Separation/DeviceN tint
/// transforms. Returns `None` for spaces it cannot resolve, letting
/// zpdf-image fall back to its own inference.
fn resolve_image_colorspace(
    file: Option<&PdfFile>,
    icc: Option<&mut zpdf_color::IccCache>,
    cs: Option<&PdfObject>,
    depth: u8,
    intent: zpdf_color::RenderIntent,
) -> Option<zpdf_image::ResolvedColorSpace> {
    use zpdf_image::ResolvedColorSpace as Rcs;
    if depth > 4 {
        return None;
    }
    let resolved;
    let cs = match cs? {
        PdfObject::Ref(r) => {
            resolved = file?.resolve(*r).ok()?;
            &resolved
        }
        other => other,
    };
    match cs {
        PdfObject::Name(n) => zpdf_image::colorspace_from_name(n.as_str()),
        PdfObject::Array(arr) => {
            let head = match arr.first()? {
                PdfObject::Name(n) => n.as_str(),
                _ => return None,
            };
            match head {
                "ICCBased" => {
                    let elem = arr.get(1)?;
                    let profile;
                    let (stream_ref, stream_obj) = match elem {
                        PdfObject::Ref(r) => {
                            profile = file?.resolve(*r).ok()?;
                            (Some(*r), &profile)
                        }
                        other => (None, other),
                    };
                    let stream = stream_obj.as_stream().ok()?;
                    let n = stream.dict.get_i64("N").ok()?;
                    if let Some(cache) = icc {
                        let transform = match (stream_ref, file) {
                            (Some(id), Some(f)) => {
                                cache.get_or_build(id, intent, || f.resolve_stream_data(id).ok())
                            }
                            (None, _) => build_inline_icc_transform(file, stream, intent),
                            _ => None,
                        };
                        if let Some(t) = transform {
                            if t.components() == n.max(1) as usize {
                                return Some(Rcs::Icc {
                                    ncomp: t.components() as u8,
                                    transform: t,
                                });
                            }
                            tracing::warn!(
                                "ICC profile has {} components but /N is {n}; using /N fallback",
                                t.components()
                            );
                        }
                    }
                    match n {
                        1 => Some(Rcs::Gray),
                        3 => Some(Rcs::Rgb),
                        4 => Some(Rcs::Cmyk),
                        n => {
                            tracing::warn!("ICCBased with unsupported /N {n}");
                            None
                        }
                    }
                }
                "Indexed" | "I" => {
                    let base = resolve_image_colorspace(file, icc, arr.get(1), depth + 1, intent)?;
                    let hival = arr.get(2)?.as_f64().ok()?;
                    if !(0.0..=255.0).contains(&hival) {
                        return None;
                    }
                    let lookup = match arr.get(3)? {
                        PdfObject::String(s) => s.as_bytes().to_vec(),
                        PdfObject::Ref(r) => file?.resolve_stream_data(*r).ok()?,
                        PdfObject::Stream(s) => {
                            decode_stream_for_file(file, &s.data, &s.dict).ok()?
                        }
                        _ => return None,
                    };
                    // Bake an ICC base into the palette (one buffer transform)
                    // so per-pixel lookups stay plain RGB.
                    let (base, lookup) = match base {
                        Rcs::Icc { transform, .. } => (Rcs::Rgb, transform.palette_to_rgb(&lookup)),
                        other => (other, lookup),
                    };
                    Some(Rcs::Indexed {
                        base: Box::new(base),
                        hival: hival as u8,
                        lookup,
                    })
                }
                "Separation" | "DeviceN" => {
                    // Separation has one colorant; DeviceN (NChannel included)
                    // one per name.
                    let n = if head == "Separation" {
                        1
                    } else {
                        let names = match arr.get(1)? {
                            PdfObject::Ref(r) => file?.resolve(*r).ok()?,
                            other => other.clone(),
                        };
                        names.as_array().ok()?.len()
                    };
                    let n = u8::try_from(n).ok().filter(|&n| n > 0)?;
                    // An alternate that cannot be resolved (or is itself a
                    // special space) leaves the tints on the polarity-correct
                    // gray fallback rather than misreading transform outputs.
                    let alternate =
                        resolve_image_colorspace(file, icc, arr.get(2), depth + 1, intent)
                            .filter(|alt| !matches!(alt, Rcs::Indexed { .. } | Rcs::Tint { .. }));
                    let transform = alternate
                        .as_ref()
                        .and(arr.get(3))
                        .and_then(|f| parse_image_function(file, f))
                        .map(std::sync::Arc::new);
                    Some(Rcs::Tint {
                        n,
                        transform,
                        alternate: Box::new(alternate.unwrap_or(Rcs::Gray)),
                    })
                }
                "CalGray" | "CalRGB" => {
                    let params = match arr.get(1)? {
                        PdfObject::Ref(r) => file?.resolve(*r).ok()?,
                        other => other.clone(),
                    };
                    let params = params.as_dict().ok()?;
                    let mut resolve = |id| file.and_then(|f| f.resolve(id).ok());
                    Some(if head == "CalGray" {
                        Rcs::CalGray(zpdf_color::CalGrayParams::from_pdf_dict(
                            params,
                            &mut resolve,
                        ))
                    } else {
                        Rcs::CalRGB(zpdf_color::CalRGBParams::from_pdf_dict(
                            params,
                            &mut resolve,
                        ))
                    })
                }
                "Lab" => {
                    let params = match arr.get(1)? {
                        PdfObject::Ref(r) => file?.resolve(*r).ok()?,
                        other => other.clone(),
                    };
                    Some(zpdf_image::lab_colorspace(params.as_dict().ok()?))
                }
                other => zpdf_image::colorspace_from_name(other),
            }
        }
        _ => None,
    }
}

/// Parse an image colour space's tint transform (stream-backed functions get
/// their decoded data). A free-function twin of the interpreter's
/// `parse_function`, for [`resolve_image_colorspace`].
fn parse_image_function(
    file: Option<&PdfFile>,
    obj: &PdfObject,
) -> Option<zpdf_color::PdfFunction> {
    let mut resolve = |id: zpdf_core::ObjectId| {
        let file = file?;
        let o = file.resolve(id).ok()?;
        let data = if o.as_stream().is_ok() {
            file.resolve_stream_data(id).ok()
        } else {
            None
        };
        // Streams must hand back their dict, not the stream object.
        let obj = match &o {
            PdfObject::Stream(s) => PdfObject::Dict(s.dict.clone()),
            other => other.clone(),
        };
        Some((obj, data))
    };
    zpdf_color::PdfFunction::parse_object(obj, &mut resolve)
}

/// Decode an image's `/SMask` through the full image-decode path (filters,
/// predictors, any bpc, /Decode) and fold it into `image` as premultiplied
/// alpha. Failures leave the image opaque rather than dropping it.
/// `colorspace` is the image's resolved colour space, which a `/Matte` colour
/// is given in (`None`: inferred from its component count).
fn fold_soft_mask(
    image: &mut zpdf_image::DecodedImage,
    smask_ref: zpdf_core::ObjectId,
    colorspace: Option<&zpdf_image::ResolvedColorSpace>,
    file: &PdfFile,
    limits: &ParseLimits,
    diagnostics: &mut zpdf_core::ProcessingDiagnostics,
) {
    let obj = match file.resolve_shared(smask_ref) {
        Ok(o) => o,
        Err(e) => {
            diagnostics.record_decode_failure(&e);
            return;
        }
    };
    let stream = match obj.as_stream() {
        Ok(s) => s,
        Err(e) => {
            diagnostics.record_decode_failure(&e);
            return;
        }
    };
    let data = match file.resolve_stream_data_shared(smask_ref) {
        Ok(d) => d,
        Err(e) => {
            diagnostics.record_decode_failure(&e);
            tracing::warn!("failed to decode /SMask stream: {e}");
            return;
        }
    };
    let dict = resolve_image_metadata(file, &stream.dict);
    // SMasks are DeviceGray by definition (spec table 144).
    match zpdf_image::decode_image_xobject_resolved_with_limits(
        &data,
        &dict,
        [0, 0, 0],
        Some(zpdf_image::ResolvedColorSpace::Gray),
        limits,
    ) {
        // A `/Matte` mask means the parent's colours were pre-blended with
        // the matte colour.
        Ok(mask) => zpdf_image::apply_smask_image_with_matte(
            image,
            &mask,
            zpdf_image::smask_matte(&dict).as_deref(),
            colorspace,
        ),
        Err(e) => {
            diagnostics.record_decode_failure(&e);
            tracing::warn!("failed to decode /SMask image: {e}");
        }
    }
}

/// Decode a `/Mask` stencil stream (1 bpc; sample 1 = masked out, `/Decode
/// [1 0]` inverts) and fold it into `image` as premultiplied alpha.
fn fold_stencil_mask(
    image: &mut zpdf_image::DecodedImage,
    mask_ref: zpdf_core::ObjectId,
    file: &PdfFile,
    limits: &ParseLimits,
    diagnostics: &mut zpdf_core::ProcessingDiagnostics,
) {
    let obj = match file.resolve_shared(mask_ref) {
        Ok(o) => o,
        Err(e) => {
            diagnostics.record_decode_failure(&e);
            return;
        }
    };
    let stream = match obj.as_stream() {
        Ok(s) => s,
        Err(_) if matches!(*obj, PdfObject::Array(_)) => return,
        Err(e) => {
            diagnostics.record_decode_failure(&e);
            return;
        }
    };
    let data = match file.resolve_stream_data_shared(mask_ref) {
        Ok(d) => d,
        Err(e) => {
            diagnostics.record_decode_failure(&e);
            tracing::warn!("failed to decode /Mask stencil stream: {e}");
            return;
        }
    };
    let dict = resolve_image_metadata(file, &stream.dict);
    let width = dict.get_i64("Width").unwrap_or(0) as u32;
    let height = dict.get_i64("Height").unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        diagnostics.malformed_content = diagnostics.malformed_content.saturating_add(1);
        return;
    }
    if u64::from(width) * u64::from(height) > limits.max_image_pixels {
        diagnostics.resource_limit_reached = true;
        return;
    }
    let invert = zpdf_image::mask_decode_inverts(&dict);
    zpdf_image::apply_stencil_mask_with_limits(image, &data, width, height, invert, limits);
}

/// Expand the abbreviated keys/values of an inline-image parameter dict to their
/// full XObject-image equivalents so the shared filter/image decoders accept it.
fn normalize_inline_image_dict(dict: &zpdf_core::PdfDict) -> zpdf_core::PdfDict {
    use zpdf_core::PdfName;
    let mut out = zpdf_core::PdfDict::new();
    for (k, v) in &dict.0 {
        let key = match k.as_str() {
            "W" => "Width",
            "H" => "Height",
            "BPC" => "BitsPerComponent",
            "CS" => "ColorSpace",
            "F" => "Filter",
            "IM" => "ImageMask",
            "D" => "Decode",
            "DP" => "DecodeParms",
            "I" => "Interpolate",
            other => other,
        };
        let value = if key == "ColorSpace" {
            normalize_cs_value(v)
        } else {
            v.clone()
        };
        out.insert(PdfName::new(key.to_string()), value);
    }
    out
}

fn normalize_cs_value(v: &PdfObject) -> PdfObject {
    use zpdf_core::PdfName;
    match v {
        PdfObject::Name(n) => PdfObject::Name(PdfName::new(expand_cs_name(n.as_str()).to_string())),
        PdfObject::Array(arr) => {
            let mut new_arr = arr.clone();
            if let Some(PdfObject::Name(n)) = new_arr.first().cloned() {
                new_arr[0] = PdfObject::Name(PdfName::new(expand_cs_name(n.as_str()).to_string()));
            }
            PdfObject::Array(new_arr)
        }
        other => other.clone(),
    }
}

fn expand_cs_name(n: &str) -> &str {
    match n {
        "G" => "DeviceGray",
        "RGB" => "DeviceRGB",
        "CMYK" => "DeviceCMYK",
        "I" => "Indexed",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_rectangle_fill() {
        let content = b"1 0 0 rg 100 200 300 400 re f";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let dl = ContentInterpreter::new(page_rect).interpret(content);
        assert_eq!(dl.commands.len(), 1);
        assert!(matches!(&dl.commands[0], RenderCommand::FillPath { .. }));
    }

    /// A path operator short of operands is skipped, not padded with zeros:
    /// padding drew strokes to the page origin.
    #[test]
    fn underfilled_path_operator_does_not_reach_the_origin() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let dl = ContentInterpreter::new(page_rect).interpret(b"100 100 m 50 l 200 200 l S");
        let RenderCommand::StrokePath { path, .. } = &dl.commands[0] else {
            panic!("expected stroke");
        };
        let points: Vec<(f64, f64)> = path.points().iter().map(|p| (p.x, p.y)).collect();
        assert_eq!(points, vec![(100.0, 100.0), (200.0, 200.0)]);
    }

    #[test]
    fn rotation_accounts_for_nonzero_box_origin() {
        let page = Rect::new(10.0, 20.0, 110.0, 220.0);
        let dl = ContentInterpreter::new(page)
            .with_page_rotation(90)
            .interpret(b"10 20 10 10 re f");
        assert_eq!(dl.page_rect, Rect::new(0.0, 0.0, 200.0, 100.0));
        let RenderCommand::FillPath { path, .. } = &dl.commands[0] else {
            panic!("expected fill");
        };
        let mut points: Vec<(f64, f64)> = path.points().iter().map(|p| (p.x, p.y)).collect();
        points.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            points,
            vec![(0.0, 90.0), (0.0, 100.0), (10.0, 90.0), (10.0, 100.0)]
        );
    }

    #[test]
    fn every_quarter_turn_maps_an_offset_box_onto_the_rotated_rect() {
        // A full-box fill must land exactly on the rotated page rect, whatever
        // the box origin, for every quarter turn (incl. normalised values).
        let page = Rect::new(10.0, 20.0, 110.0, 220.0);
        for rotate in [90, 180, 270, -90, 450] {
            let dl = ContentInterpreter::new(page)
                .with_page_rotation(rotate)
                .interpret(b"10 20 100 200 re f");
            let RenderCommand::FillPath { path, .. } = &dl.commands[0] else {
                panic!("expected fill");
            };
            let r = dl.page_rect;
            assert_eq!((r.x0, r.y0), (0.0, 0.0), "rotate {rotate}");
            for p in path.points() {
                let on_x = (p.x - r.x0).abs() < 1e-9 || (p.x - r.x1).abs() < 1e-9;
                let on_y = (p.y - r.y0).abs() < 1e-9 || (p.y - r.y1).abs() < 1e-9;
                assert!(on_x && on_y, "rotate {rotate}: {p:?} not a corner of {r:?}");
            }
        }
    }

    #[test]
    fn interpret_save_restore() {
        let content = b"q 0.5 g 100 100 50 50 re f Q 100 100 50 50 re f";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let dl = ContentInterpreter::new(page_rect).interpret(content);
        assert_eq!(dl.commands.len(), 2);
    }

    // ---- Text clipping (render modes 4–7) ----

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // A minimal 8-glyph TrueType font carrying real contours for glyphs 3..=6,
    // used as an Identity-CID composite so `<0003>` etc. resolve to those GIDs.
    const FONT_PRESENT_HEX: &str = concat!(
        "00010000000a0080000300204f532f32412841d30000012800000060636d6170000c008a0000019c00000034676c7966",
        "9c789c74000001e400000068686561642e66b048000000ac000000366868656104b2025a000000e400000024686d7478",
        "0258000000000188000000126c6f636100680071000001d0000000126d617870000a000600000108000000206e616d65",
        "00995cc80000024c0000003c706f7374d625404700000288000000470001000000010000d0fc3df65f0f3cf5000103e8",
        "00000000e67d365600000000e67d36560064000001f402bc000000030002000000000000000100000320ff3800000258",
        "000000c80190000100000000000000000000000000000001000100000008000400010000000000020000000000000000",
        "000000000000000000030258019000050004000000000000000000000000000000000000000000000000000000000000",
        "0000000000010000000000000000000000003f3f3f3f0000003100370000000000000000000000000000000000000000",
        "000000000020000002580000000000000000000000000000000000000000000200000003000000140003000100000014",
        "00040020000000040004000100000037ffff00000031ffffffd00001000000000000000000000000000d001a00270034",
        "0034000000010064000001f402bc000300003311211164019002bcfd440000010064000001f402bc0003000033112111",
        "64019002bcfd440000010064000001f402bc000300003311211164019002bcfd440000010064000001f402bc00030000",
        "3311211164019002bcfd4400000000040036000100000000000100010000000100000000000200010001000300010409",
        "000100020002000300010409000200020004545200540052000200000000000000000000000000000000000000000000",
        "000000000000000000080000010201030104010501060107010802673102673202673302673402673502673602673700",
    );

    #[test]
    fn text_clip_mode_confines_following_fill_to_glyphs() {
        // PDF text render modes 4–7 add the shown glyph outlines to the clipping
        // path, applied at ET. Without this a following opaque fill/image that
        // was meant to appear only through the glyph shapes paints over the whole
        // surrounding region. A `7 Tr` (clip-only) show followed by a
        // page-covering fill inside one q…Q must therefore emit a text-clip
        // PushClip before the fill (popped by the enclosing Q), while the
        // clip-only text itself paints no visible glyph run.
        let kinds = |content: &[u8]| -> Vec<&'static str> {
            let mut fonts = FontCache::new();
            let mut f = zpdf_font::LoadedFont::new_with_data(
                zpdf_font::PdfFontType::Type0CidType2,
                "Sub".into(),
                hex_bytes(FONT_PRESENT_HEX),
                zpdf_font::CidWidths::new(1000.0),
            );
            f.cid_cmap = Some(zpdf_font::cmap::CidCMap::identity(0));
            fonts.insert("F".to_string(), f);
            let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 612.0, 792.0))
                .with_fonts(&mut fonts)
                .interpret(content);
            dl.commands
                .iter()
                .map(|c| match c {
                    RenderCommand::PushClip { .. } => "push",
                    RenderCommand::PopClip => "pop",
                    RenderCommand::FillPath { .. } => "fill",
                    RenderCommand::DrawGlyphRun(_) => "glyph",
                    _ => "other",
                })
                .collect()
        };

        let clip_kinds = kinds(b"q BT /F 40 Tf 7 Tr <0003000400050006> Tj ET 0 0 1000 1000 re f Q");
        // Baseline: identical but ordinary fill text (mode 0) — no text clip.
        let base_kinds = kinds(b"q BT /F 40 Tf 0 Tr <0003000400050006> Tj ET 0 0 1000 1000 re f Q");

        assert!(
            !clip_kinds.contains(&"glyph"),
            "mode-7 text paints nothing: {clip_kinds:?}"
        );
        assert!(
            base_kinds.contains(&"glyph"),
            "mode-0 text paints a glyph run: {base_kinds:?}"
        );

        let fi = clip_kinds.iter().position(|k| *k == "fill").expect("fill");
        assert!(
            clip_kinds[..fi].contains(&"push"),
            "text clip pushed before the fill: {clip_kinds:?}"
        );
        let bfi = base_kinds.iter().position(|k| *k == "fill").expect("fill");
        assert!(
            !base_kinds[..bfi].contains(&"push"),
            "no clip without a clip render mode: {base_kinds:?}"
        );

        assert_eq!(
            clip_kinds.iter().filter(|k| **k == "push").count(),
            clip_kinds.iter().filter(|k| **k == "pop").count(),
            "clips balanced: {clip_kinds:?}"
        );
    }

    #[test]
    fn clip_text_with_an_unreadable_outline_does_not_hide_later_paint() {
        // An impossible contour count makes glyph 3 unreadable. Unlike a valid
        // blank glyph, it says nothing about the clip area, so no empty clip
        // may hide the page-covering fill that follows.
        let mut bytes = hex_bytes(FONT_PRESENT_HEX);
        let table = |bytes: &[u8], tag: &[u8; 4]| {
            let count = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
            let record = (0..count)
                .map(|index| 12 + index * 16)
                .find(|&record| &bytes[record..record + 4] == tag)
                .expect("table");
            u32::from_be_bytes(bytes[record + 8..record + 12].try_into().unwrap()) as usize
        };
        let (glyf, loca, head) = (
            table(&bytes, b"glyf"),
            table(&bytes, b"loca"),
            table(&bytes, b"head"),
        );
        assert_eq!(&bytes[head + 50..head + 52], &[0, 0], "short loca");
        let offset = u16::from_be_bytes([bytes[loca + 6], bytes[loca + 7]]) as usize * 2;
        bytes[glyf + offset..glyf + offset + 2].copy_from_slice(&32767i16.to_be_bytes());
        let mut font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type0CidType2,
            "Broken".into(),
            bytes,
            zpdf_font::CidWidths::new(1000.0),
        );
        font.cid_cmap = Some(zpdf_font::cmap::CidCMap::identity(0));
        assert!(font.glyph_outline(3).is_none() && !font.is_blank_glyph(3));
        let mut fonts = FontCache::new();
        fonts.insert("F".to_string(), font);
        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 612.0, 792.0))
            .with_fonts(&mut fonts)
            .interpret(b"q BT /F 40 Tf 7 Tr <0003> Tj ET 0 0 1000 1000 re f Q");
        assert!(
            !dl.commands
                .iter()
                .any(|command| matches!(command, RenderCommand::PushClip { .. })),
            "no clip from an unreadable outline"
        );
    }

    #[test]
    fn with_content_translation_shifts_emitted_geometry() {
        // with_content_translation applies a (dx, dy) shift as the initial CTM,
        // exactly like a leading `1 0 0 1 dx dy cm`, without prepending that
        // operator to a copy of the whole content stream. Emitted geometry must
        // move by exactly (dx, dy) versus an untranslated interpret.
        let content = b"10 20 5 5 re f";
        let page = Rect::new(0.0, 0.0, 100.0, 100.0);
        let first_move = |dl: &DisplayList| -> (f64, f64) {
            for c in &dl.commands {
                if let RenderCommand::FillPath { path, .. } = c {
                    if let Some(PathElement::MoveTo(p)) = path.iter().next() {
                        return (p.x, p.y);
                    }
                }
            }
            panic!("no fill emitted");
        };
        let base = first_move(&ContentInterpreter::new(page).interpret(content));
        let shifted = first_move(
            &ContentInterpreter::new(page)
                .with_content_translation(100.0, 200.0)
                .interpret(content),
        );
        assert!(
            (shifted.0 - (base.0 + 100.0)).abs() < 1e-6
                && (shifted.1 - (base.1 + 200.0)).abs() < 1e-6,
            "content shifted by exactly (100, 200): base={base:?} shifted={shifted:?}"
        );
    }

    /// A one-object file holding an 8x8 `/ImageMask` stencil XObject.
    fn stencil_image_file() -> (PdfFile, ObjectId) {
        let samples = [0b1010_1010u8, 0b0101_0101].repeat(4);
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let offset = pdf.len();
        pdf.extend_from_slice(
            format!(
                "1 0 obj\n<< /Type /XObject /Subtype /Image /Width 8 /Height 8 \
                 /ImageMask true /BitsPerComponent 1 /Length {} >>\nstream\n",
                samples.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&samples);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n0 2\n0000000000 65535 f \n{offset:010} 00000 n \n\
                 trailer\n<< /Size 2 >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        (PdfFile::parse(pdf).expect("stencil file"), ObjectId(1, 0))
    }

    #[test]
    fn repeated_stencil_draws_decode_once_per_fill_colour() {
        let (file, id) = stencil_image_file();
        let object = file.resolve(id).expect("stencil object");
        let stream = object.as_stream().expect("stencil stream");
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &resources)
            .with_images(&mut cache);
        for (color, x) in [
            (Color::black(), 0.0),
            (Color::black(), 10.0),
            (Color::rgb(1.0, 0.0, 0.0), 20.0),
            (Color::black(), 30.0),
            (Color::rgb(1.0, 0.0, 0.0), 40.0),
        ] {
            interp.current.fill_color = color;
            interp.current.ctm = Matrix::new(8.0, 0.0, 0.0, 8.0, x, 0.0);
            interp.do_image_xobject(id, stream);
        }
        let draws: Vec<_> = interp
            .display_list
            .commands
            .iter()
            .filter_map(|command| match command {
                RenderCommand::DrawImage(draw) => Some((draw.image_id, draw.is_image_mask)),
                _ => None,
            })
            .collect();
        assert_eq!(draws.len(), 5);
        assert!(draws.iter().all(|&(_, mask)| mask));
        assert_eq!(draws[0].0, draws[1].0, "same colour reuses the stencil");
        assert_eq!(draws[0].0, draws[3].0);
        assert_eq!(draws[2].0, draws[4].0);
        assert_ne!(draws[0].0, draws[2].0, "another colour is its own stencil");
        let images = interp.image_cache.as_deref().expect("image cache");
        assert_eq!(images.len(), 2, "one stencil per fill colour");
        let red = images.get(draws[2].0).expect("red stencil");
        assert!(red.data.chunks(4).any(|px| px == [255, 0, 0, 255]));
    }

    // ---- Image footprints for the CPU backend (`with_image_downscale`) ----

    const FOOTPRINT_SOURCE: u32 = 64;

    /// A one-object file holding a 64x64 DeviceGray image XObject whose
    /// samples are not periodic, so filtering a filtered copy differs from
    /// filtering the source.
    fn footprint_image_file() -> (PdfFile, ObjectId) {
        let size = FOOTPRINT_SOURCE as usize;
        let samples: Vec<u8> = (0..size * size)
            .map(|i| ((i * 37 + (i / size) * 11) % 251) as u8)
            .collect();
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let offset = pdf.len();
        pdf.extend_from_slice(
            format!(
                "1 0 obj\n<< /Type /XObject /Subtype /Image /Width {size} /Height {size} \
                 /ColorSpace /DeviceGray /BitsPerComponent 8 /Length {} >>\nstream\n",
                samples.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&samples);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n0 2\n0000000000 65535 f \n{offset:010} 00000 n \n\
                 trailer\n<< /Size 2 >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        (PdfFile::parse(pdf).expect("image file"), ObjectId(1, 0))
    }

    /// Draw the image at `side`×`side` points at the origin.
    fn draw_footprint(
        interp: &mut ContentInterpreter<'_>,
        file: &PdfFile,
        id: ObjectId,
        side: f64,
    ) {
        let object = file.resolve(id).expect("image object");
        let stream = object.as_stream().expect("image stream");
        interp.current.ctm = Matrix::new(side, 0.0, 0.0, side, 0.0, 0.0);
        interp.do_image_xobject(id, stream);
    }

    /// Stored size of each emitted draw, in order.
    fn drawn_sizes(interp: &ContentInterpreter<'_>) -> Vec<(u32, u32)> {
        let cache = interp.image_cache.as_deref().expect("image cache");
        interp
            .display_list
            .commands
            .iter()
            .filter_map(|command| match command {
                RenderCommand::DrawImage(draw) => {
                    let image = cache.get(draw.image_id).expect("admitted image");
                    Some((image.width, image.height))
                }
                _ => None,
            })
            .collect()
    }

    fn footprint_interpreter<'a>(
        file: &'a PdfFile,
        resources: &'a ResourceDict,
        cache: &'a mut ImageCache,
    ) -> ContentInterpreter<'a> {
        ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(file, resources)
            .with_images(cache)
            .with_image_downscale(1.0)
    }

    #[test]
    fn footprints_are_filtered_from_the_retained_source() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        let mut interp = footprint_interpreter(&file, &resources, &mut cache);
        // 64 samples over 30 points is below 0.5x, so every draw is minified.
        for side in [30.0, 20.0, 10.0, 30.0, 20.0, 10.0] {
            draw_footprint(&mut interp, &file, id, side);
        }
        assert_eq!(
            drawn_sizes(&interp),
            [(30, 30), (20, 20), (10, 10), (30, 30), (20, 20), (10, 10)],
        );
        let source = interp.image_sources.get(&id).expect("source entry");
        assert_eq!(
            source.decodes, 2,
            "the 20pt draw decodes again; 10pt does not"
        );
        assert!(!source.unfiltered);
        let pixels = source.retained.as_ref().expect("retained source");
        assert_eq!(interp.retained_image_bytes, source.bytes);
        let images = interp.image_cache.as_deref().expect("image cache");
        assert_eq!(images.len(), 3, "one cached copy per size, no source");
        for side in [30, 20, 10] {
            let copy = images
                .get(interp.image_obj_cache[&(id, side, side)])
                .expect("filtered copy");
            assert_eq!(
                copy.data,
                zpdf_image::box_downscale(pixels, side, side).data,
                "{side}x{side} is filtered from the source, not from another copy"
            );
        }
        let refiltered =
            zpdf_image::box_downscale(&zpdf_image::box_downscale(pixels, 30, 30), 10, 10);
        assert_ne!(
            refiltered.data,
            zpdf_image::box_downscale(pixels, 10, 10).data,
            "the fixture distinguishes double filtering"
        );
    }

    #[test]
    fn an_unminified_draw_caches_the_source_for_new_sizes() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        let mut interp = footprint_interpreter(&file, &resources, &mut cache);
        // 40 points is above 0.5x: the second draw needs the source itself.
        for side in [30.0, 40.0, 10.0, 30.0] {
            draw_footprint(&mut interp, &file, id, side);
        }
        assert_eq!(
            drawn_sizes(&interp),
            [(30, 30), (64, 64), (64, 64), (30, 30)],
            "a larger draw never reuses a smaller copy; new sizes use the cached source"
        );
        let source = &interp.image_sources[&id];
        assert_eq!(source.decodes, 2);
        assert!(source.unfiltered && source.retained.is_none());
        assert_eq!(interp.retained_image_bytes, 0);
    }

    #[test]
    fn large_then_small_draws_the_cached_source_without_decoding() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        let mut interp = footprint_interpreter(&file, &resources, &mut cache);
        for side in [64.0, 10.0, 25.0] {
            draw_footprint(&mut interp, &file, id, side);
        }
        assert_eq!(drawn_sizes(&interp), [(64, 64); 3]);
        assert_eq!(interp.image_sources[&id].decodes, 1);
        assert_eq!(interp.image_cache.as_deref().map(ImageCache::len), Some(1));
    }

    #[test]
    fn minified_only_images_never_cache_their_source() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        {
            let mut interp = footprint_interpreter(&file, &resources, &mut cache);
            for _ in 0..3 {
                draw_footprint(&mut interp, &file, id, 16.0);
            }
            assert_eq!(drawn_sizes(&interp), [(16, 16); 3]);
            assert_eq!(interp.image_sources[&id].decodes, 1);
        }
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes_used(), 16 * 16 * 4);
    }

    #[test]
    fn without_downscale_every_draw_shares_the_decoded_source() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &resources)
            .with_images(&mut cache);
        for side in [10.0, 64.0, 30.0] {
            draw_footprint(&mut interp, &file, id, side);
        }
        assert_eq!(drawn_sizes(&interp), [(64, 64); 3]);
        assert_eq!(interp.image_sources[&id].decodes, 1);
        assert_eq!(interp.image_cache.as_deref().map(ImageCache::len), Some(1));
    }

    #[test]
    fn under_the_byte_limit_every_draw_without_the_option_still_draws() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let source_bytes = (FOOTPRINT_SOURCE * FOOTPRINT_SOURCE * 4) as u64;
        // The source fits, so without the option every draw shares it. With
        // the option the retained source and three filtered copies fill the
        // limit; the fourth copy does not fit and the source takes over.
        let limit = source_bytes + (30 * 30 + 28 * 28 + 26 * 26) * 4;
        let sides = [30.0, 28.0, 26.0, 24.0, 22.0];
        let mut plain_cache = ImageCache::new();
        let mut plain = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &resources)
            .with_images(&mut plain_cache)
            .with_image_cache_limit(limit);
        let mut cache = ImageCache::new();
        let mut interp =
            footprint_interpreter(&file, &resources, &mut cache).with_image_cache_limit(limit);
        for side in sides {
            draw_footprint(&mut plain, &file, id, side);
            draw_footprint(&mut interp, &file, id, side);
        }
        assert_eq!(drawn_sizes(&plain).len(), sides.len());
        assert_eq!(
            drawn_sizes(&interp),
            [(30, 30), (28, 28), (26, 26), (64, 64), (64, 64)]
        );
        assert_eq!(interp.retained_image_bytes, 0);
        let used = interp
            .image_cache
            .as_deref()
            .map_or(0, ImageCache::bytes_used);
        assert!(used <= limit);
        assert_eq!(interp.image_sources[&id].decodes, 2);
    }

    #[test]
    fn filtered_copies_never_outgrow_their_source() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        let mut interp = footprint_interpreter(&file, &resources, &mut cache);
        // Thirty minified sizes total 2.5 times the source.
        for side in (2..32).rev() {
            draw_footprint(&mut interp, &file, id, side as f64);
        }
        assert_eq!(drawn_sizes(&interp).len(), 30);
        let source = &interp.image_sources[&id];
        assert!(source.unfiltered, "the source replaced further copies");
        assert!(source.filtered_bytes <= source.bytes);
        let used = interp
            .image_cache
            .as_deref()
            .map_or(0, ImageCache::bytes_used);
        assert!(
            used + interp.retained_image_bytes <= 2 * source.bytes,
            "{used} cached for a {}-byte source",
            source.bytes
        );
        assert_eq!(source.decodes, 2);
    }

    #[test]
    fn a_source_without_room_is_never_decoded_a_third_time() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        // Room for the filtered copies, not for the 16 KiB source. Without
        // the option nothing draws; with it the cached sizes still do.
        let mut interp =
            footprint_interpreter(&file, &resources, &mut cache).with_image_cache_limit(4096);
        for side in [10.0, 20.0, 15.0, 10.0, 20.0, 12.0] {
            draw_footprint(&mut interp, &file, id, side);
        }
        assert_eq!(
            drawn_sizes(&interp),
            [(10, 10), (20, 20), (10, 10), (20, 20)],
            "new sizes after the second decode are skipped; cached sizes still draw"
        );
        assert_eq!(interp.image_sources[&id].decodes, 2);
        assert!(interp.image_sources[&id].retained.is_none());
        assert!(interp.rejected_image_objects.contains(&id));
    }

    #[test]
    fn a_second_decode_keeps_the_first_rendering_intent() {
        let (file, id) = footprint_image_file();
        let resources = ResourceDict::default();
        let mut cache = ImageCache::new();
        let mut interp = footprint_interpreter(&file, &resources, &mut cache);
        draw_footprint(&mut interp, &file, id, 30.0);
        interp.current.rendering_intent = zpdf_color::RenderIntent::Perceptual;
        draw_footprint(&mut interp, &file, id, 20.0);
        let source = &interp.image_sources[&id];
        assert_eq!(source.decodes, 2);
        assert_eq!(
            source.intent,
            zpdf_color::RenderIntent::RelativeColorimetric
        );
    }

    #[test]
    fn stored_footprints_are_never_filtered_again() {
        // Every stored size is the source or a target the CPU backend samples
        // directly, so the backend filters at most once, from the source.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut filtered, mut refiltering) = (0, 0);
        for scale in [0.5f32, 1.0, 1.333_333_4, 150.0 / 72.0, 300.0 / 72.0] {
            let interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
                .with_image_downscale(scale);
            for sample in 0..60_000 {
                let width = (next() % 4000 + 1) as u32;
                let height = (next() % 4000 + 1) as u32;
                let transform = if sample % 2 == 0 {
                    // One axis under half a device pixel, the other at an
                    // integer or fractional length: where refiltering rounds.
                    let thin = (next() % 1000) as f64 / 2000.0 / scale as f64;
                    let long = (next() % 4000) as f64 / scale as f64
                        + if next() % 2 == 0 {
                            0.0
                        } else {
                            (next() % 1000) as f64 / 1000.0
                        };
                    Matrix::new(thin, 0.0, 0.0, long, 0.0, 0.0)
                } else {
                    let a = (next() % 4_000_000) as f64 / 1000.0;
                    let b = (next() % 100) as f64;
                    let d = (next() % 4_000_000) as f64 / 1000.0;
                    Matrix::new(a, b, 0.0, d, 0.0, 0.0)
                };
                let stored = interp.image_footprint(&transform, width, height);
                let target = zpdf_image::box_filter_size(&transform, scale, width, height);
                match target {
                    Some((w, h))
                        if zpdf_image::box_filter_size(&transform, scale, w, h).is_some() =>
                    {
                        refiltering += 1;
                        assert_eq!(
                            stored,
                            (width, height),
                            "{width}x{height} under {transform:?} at {scale}: the backend \
                             would filter {:?} again",
                            (w, h)
                        );
                    }
                    Some(size) => {
                        filtered += 1;
                        assert_eq!(stored, size, "a fixed-point target is stored");
                    }
                    None => assert_eq!(stored, (width, height)),
                }
            }
        }
        assert!(
            filtered > 10_000,
            "the sample stores footprints: {filtered}"
        );
        assert!(refiltering > 0, "the sample reaches refiltering targets");
    }

    // ---- Ruled-line capture (rule sink for table detection) ----

    #[test]
    fn rule_sink_captures_thin_strokes_and_rect_fills() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        // A horizontal stroked line, a vertical stroked line, a thin filled
        // rect (horizontal rule drawn with `re f`), and a fat rect (not a rule).
        let content = b"0.5 w 50 700 m 400 700 l S \
                        200 600 m 200 750 l S \
                        50 649.6 350 0.8 re f \
                        50 100 200 300 re f";
        let mut rules: Vec<RuleLine> = Vec::new();
        let dl = ContentInterpreter::new(page_rect)
            .with_rule_sink(&mut rules)
            .interpret(content);
        assert_eq!(rules.len(), 3, "two strokes + one thin rect: {rules:?}");
        assert!(!rules[0].vertical && (rules[0].pos - 700.0).abs() < 0.01);
        assert!(rules[1].vertical && (rules[1].pos - 200.0).abs() < 0.01);
        assert!(!rules[2].vertical && (rules[2].pos - 650.0).abs() < 0.01);
        assert!((rules[2].start - 50.0).abs() < 0.01 && (rules[2].end - 400.0).abs() < 0.01);
        // Rendering output is untouched: 2 strokes + 2 fills.
        assert_eq!(dl.commands.len(), 4);
    }

    #[test]
    fn rule_sink_ignores_thick_strokes_curves_and_short_ticks() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let content = b"8 w 50 700 m 400 700 l S \
                        0.5 w 50 600 m 100 650 l S \
                        50 500 m 60 520 70 540 80 560 c S \
                        50 400 m 54 400 l S";
        let mut rules: Vec<RuleLine> = Vec::new();
        ContentInterpreter::new(page_rect)
            .with_rule_sink(&mut rules)
            .interpret(content);
        assert!(
            rules.is_empty(),
            "thick/diagonal/curved/short marks are not rules: {rules:?}"
        );
    }

    #[test]
    fn rule_sink_does_not_change_display_list() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let content = b"0.5 w 50 700 m 400 700 l S 50 649.6 350 0.8 re f";
        let without = ContentInterpreter::new(page_rect).interpret(content);
        let mut rules: Vec<RuleLine> = Vec::new();
        let with = ContentInterpreter::new(page_rect)
            .with_rule_sink(&mut rules)
            .interpret(content);
        assert_eq!(
            format!("{:?}", without.commands),
            format!("{:?}", with.commands),
            "rule sink must not change render commands"
        );
        assert!(!rules.is_empty());
    }

    // ---- Output intents: DeviceCMYK colour-managed through /DestOutputProfile ----

    /// A 4-channel (CMYK) ICC transform from the committed `cmyk_lut.icc`
    /// fixture, or `None` if the fixture is absent in this checkout (skip).
    fn cmyk_oi_transform() -> Option<std::sync::Arc<zpdf_color::IccTransform>> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../zpdf-color/src/testdata/cmyk_lut.icc"
        );
        let bytes = std::fs::read(path).ok()?;
        let t = zpdf_color::IccTransform::from_profile_bytes(
            &bytes,
            zpdf_color::RenderIntent::default(),
        )
        .ok()?;
        assert_eq!(t.components(), 4, "fixture must be a CMYK profile");
        Some(std::sync::Arc::new(t))
    }

    fn first_fill_color(dl: &DisplayList) -> Color {
        for cmd in &dl.commands {
            if let RenderCommand::FillPath {
                paint: Paint::Solid(c),
                ..
            } = cmd
            {
                return *c;
            }
        }
        panic!("no solid FillPath in display list");
    }

    fn approx(a: f32, b: f64) -> bool {
        (a - b as f32).abs() < 0.01
    }

    /// The `k` operator routes DeviceCMYK through the output-intent profile, not
    /// the SWOP polynomial — and the two differ.
    #[test]
    fn devicecmyk_k_routes_through_output_intent() {
        let Some(oi) = cmyk_oi_transform() else {
            return;
        };
        let page = Rect::new(0.0, 0.0, 100.0, 100.0);
        // K=1 (black via the K colorant): SWOP renders a dark grey, the LUT
        // profile a deeper near-black — a clear, stable discriminator.
        let content = b"0 0 0 1 k 0 0 10 10 re f";
        let dl = ContentInterpreter::new(page)
            .with_output_intent_cmyk(oi.clone())
            .interpret(content);
        let got = first_fill_color(&dl);
        let (er, eg, eb) = oi.color_to_rgb(&[0.0, 0.0, 0.0, 1.0]);
        assert!(
            approx(got.r, er) && approx(got.g, eg) && approx(got.b, eb),
            "k did not route through the OI profile: got {got:?}, want ({er},{eg},{eb})"
        );
        let (sr, sg, sb) = zpdf_color::cmyk_to_rgb(0.0, 0.0, 0.0, 1.0);
        assert!(
            (got.r - sr as f32).abs() > 0.02
                || (got.g - sg as f32).abs() > 0.02
                || (got.b - sb as f32).abs() > 0.02,
            "OI render unexpectedly equals the SWOP polynomial: {got:?}"
        );
    }

    /// `scn` in an explicitly selected DeviceCMYK space also routes through the
    /// OI profile (the `components_to_rgb` conversion site).
    #[test]
    fn devicecmyk_scn_routes_through_output_intent() {
        let Some(oi) = cmyk_oi_transform() else {
            return;
        };
        let page = Rect::new(0.0, 0.0, 100.0, 100.0);
        let content = b"/DeviceCMYK cs 0.1 0.2 0.3 0.4 scn 0 0 10 10 re f";
        let dl = ContentInterpreter::new(page)
            .with_output_intent_cmyk(oi.clone())
            .interpret(content);
        let got = first_fill_color(&dl);
        let (er, eg, eb) = oi.color_to_rgb(&[0.1, 0.2, 0.3, 0.4]);
        assert!(
            approx(got.r, er) && approx(got.g, eg) && approx(got.b, eb),
            "scn DeviceCMYK did not route through the OI profile: got {got:?}"
        );
    }

    /// Without an output intent, DeviceCMYK keeps the SWOP polynomial exactly —
    /// the gate that guarantees non-OI documents render byte-identically.
    #[test]
    fn devicecmyk_without_output_intent_uses_swop() {
        let page = Rect::new(0.0, 0.0, 100.0, 100.0);
        let content = b"0 0 0 1 k 0 0 10 10 re f";
        let dl = ContentInterpreter::new(page).interpret(content);
        let got = first_fill_color(&dl);
        let (sr, sg, sb) = zpdf_color::cmyk_to_rgb(0.0, 0.0, 0.0, 1.0);
        assert!(
            (got.r - sr as f32).abs() < 0.001
                && (got.g - sg as f32).abs() < 0.001
                && (got.b - sb as f32).abs() < 0.001,
            "no-OI CMYK must use the SWOP polynomial unchanged: {got:?}"
        );
    }

    /// A DeviceCMYK image space is upgraded to the OI profile's `Icc` space, and
    /// an Indexed/DeviceCMYK palette is baked to RGB through the profile.
    #[test]
    fn cmyk_image_colorspace_upgraded_to_output_intent() {
        use zpdf_image::ResolvedColorSpace as Rcs;
        let Some(oi) = cmyk_oi_transform() else {
            return;
        };
        let interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
            .with_output_intent_cmyk(oi.clone());

        match interp.colormanage_cmyk_image(Some(Rcs::Cmyk)) {
            Some(Rcs::Icc { ncomp, transform }) => {
                assert_eq!(ncomp, 4);
                assert!(std::sync::Arc::ptr_eq(&transform, &oi));
            }
            other => panic!("bare DeviceCMYK image must become Icc(4): {other:?}"),
        }

        let indexed = Rcs::Indexed {
            base: Box::new(Rcs::Cmyk),
            hival: 0,
            lookup: vec![0, 0, 0, 255], // one CMYK palette entry (K=1)
        };
        match interp.colormanage_cmyk_image(Some(indexed)) {
            Some(Rcs::Indexed { base, lookup, .. }) => {
                assert!(matches!(*base, Rcs::Rgb), "CMYK base must bake to RGB");
                // Baked through the OI profile, byte-for-byte — not SWOP.
                assert_eq!(
                    lookup,
                    oi.palette_to_rgb(&[0, 0, 0, 255]),
                    "palette must bake through the OI profile"
                );
                let (sr, sg, sb) = zpdf_color::cmyk_to_rgb(0.0, 0.0, 0.0, 1.0);
                let swop = [
                    (sr * 255.0).round() as u8,
                    (sg * 255.0).round() as u8,
                    (sb * 255.0).round() as u8,
                ];
                assert_ne!(
                    lookup.as_slice(),
                    swop.as_slice(),
                    "OI-baked palette must differ from the SWOP polynomial"
                );
            }
            other => panic!("Indexed/CMYK base must bake through the profile: {other:?}"),
        }

        // A non-CMYK image space is untouched.
        assert!(matches!(
            interp.colormanage_cmyk_image(Some(Rcs::Rgb)),
            Some(Rcs::Rgb)
        ));
    }

    /// Without an output intent, image colour spaces pass through unchanged.
    #[test]
    fn cmyk_image_unchanged_without_output_intent() {
        use zpdf_image::ResolvedColorSpace as Rcs;
        let interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        assert!(matches!(
            interp.colormanage_cmyk_image(Some(Rcs::Cmyk)),
            Some(Rcs::Cmyk)
        ));
    }

    /// Invariant: an image colour space that already carries a compiled embedded
    /// ICC profile keeps *that* profile — the output intent substitutes only for
    /// raw DeviceCMYK, never for an explicit ICCBased space. Proven by pointer
    /// identity (the two transforms share bytes but are distinct allocations).
    #[test]
    fn embedded_icc_image_keeps_its_own_profile_under_output_intent() {
        use zpdf_image::ResolvedColorSpace as Rcs;
        let (Some(embedded), Some(oi)) = (cmyk_oi_transform(), cmyk_oi_transform()) else {
            return;
        };
        assert!(!std::sync::Arc::ptr_eq(&embedded, &oi));
        let interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
            .with_output_intent_cmyk(oi.clone());
        match interp.colormanage_cmyk_image(Some(Rcs::Icc {
            ncomp: 4,
            transform: embedded.clone(),
        })) {
            Some(Rcs::Icc { transform, .. }) => {
                assert!(
                    std::sync::Arc::ptr_eq(&transform, &embedded),
                    "embedded image ICC profile must be kept, not swapped for the OI"
                );
                assert!(!std::sync::Arc::ptr_eq(&transform, &oi));
            }
            other => panic!("embedded Icc(4) image space must pass through: {other:?}"),
        }
    }

    /// Invariant (vector/text path): a fill in an explicit embedded ICCBased(4)
    /// colour space renders through that profile, never the output intent. The
    /// two transforms are the same fixture under *different* rendering intents,
    /// so when the profile carries distinct A2B tables the OI value is provably
    /// not used.
    #[test]
    fn embedded_icc_vector_keeps_its_own_profile_under_output_intent() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../zpdf-color/src/testdata/cmyk_lut.icc"
        );
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let mk = |intent| {
            std::sync::Arc::new(
                zpdf_color::IccTransform::from_profile_bytes(&bytes, intent).unwrap(),
            )
        };
        let embedded = mk(zpdf_color::RenderIntent::Perceptual);
        let oi = mk(zpdf_color::RenderIntent::RelativeColorimetric);
        let interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
            .with_output_intent_cmyk(oi.clone());

        let vals = [0.1, 0.55, 0.3, 0.2];
        let got = interp.components_to_rgb(&ActiveColorSpace::Icc(embedded.clone()), &vals);
        let (er, eg, eb) = embedded.color_to_rgb(&vals);
        assert!(
            approx(got.r, er) && approx(got.g, eg) && approx(got.b, eb),
            "embedded ICC vector colour must use its own profile: got {got:?}"
        );
        // When the two intents resolve this colour differently, prove the result
        // is the embedded profile's value, not the output intent's.
        let (oir, oig, oib) = oi.color_to_rgb(&vals);
        if (er - oir).abs() > 0.004 || (eg - oig).abs() > 0.004 || (eb - oib).abs() > 0.004 {
            assert!(
                !(approx(got.r, oir) && approx(got.g, oig) && approx(got.b, oib)),
                "embedded ICC must not be rerouted through the output intent"
            );
        }
    }

    /// Nonzero winding number of a line-only path around `at`.
    fn winding(path: &Path, at: Point) -> i32 {
        let mut polygons: Vec<Vec<Point>> = Vec::new();
        for element in path {
            match element {
                PathElement::MoveTo(p) => polygons.push(vec![p]),
                PathElement::LineTo(p) => polygons.last_mut().unwrap().push(p),
                PathElement::CurveTo(..) => panic!("line-only test path"),
                PathElement::Close => {}
            }
        }
        let mut winding = 0;
        for polygon in polygons {
            for (i, &a) in polygon.iter().enumerate() {
                let b = polygon[(i + 1) % polygon.len()];
                let side = (b.x - a.x) * (at.y - a.y) - (at.x - a.x) * (b.y - a.y);
                if a.y <= at.y && b.y > at.y && side > 0.0 {
                    winding += 1;
                } else if a.y > at.y && b.y <= at.y && side < 0.0 {
                    winding -= 1;
                }
            }
        }
        winding
    }

    fn square(path: &mut Path, x0: f64, y0: f64, size: f64, clockwise: bool) {
        let corners = [(0.0, 0.0), (0.0, size), (size, size), (size, 0.0)];
        path.move_to(Point::new(x0, y0));
        for &(dx, dy) in &corners[1..] {
            let (dx, dy) = if clockwise { (dx, dy) } else { (dy, dx) };
            path.line_to(Point::new(x0 + dx, y0 + dy));
        }
        path.close();
    }

    #[test]
    fn glyph_contours_union_regardless_of_font_winding() {
        // A TrueType-style clockwise glyph overlapping a CFF-style
        // counter-clockwise one: concatenated, the overlap winds to zero.
        let (mut first, mut second) = (Path::new(), Path::new());
        square(&mut first, 0.0, 0.0, 10.0, true);
        orient_glyph_contours(&mut first);
        square(&mut second, 5.0, 5.0, 10.0, false);
        orient_glyph_contours(&mut second);
        let overlap = Point::new(7.0, 7.0);
        assert_eq!(winding(&first, overlap), 1);
        assert_eq!(winding(&second, overlap), 1);
        let mut path = first;
        path.extend_from_path(&second);
        assert_eq!(winding(&path, overlap), 2);
        assert_eq!(winding(&path, Point::new(2.0, 2.0)), 1);
        assert_eq!(winding(&path, Point::new(20.0, 2.0)), 0);
    }

    #[test]
    fn reversing_a_glyph_keeps_its_counters() {
        // Clockwise outer contour with a counter-clockwise hole (TrueType 'O').
        let mut path = Path::new();
        square(&mut path, 0.0, 0.0, 10.0, true);
        square(&mut path, 3.0, 3.0, 4.0, false);
        orient_glyph_contours(&mut path);
        assert_eq!(path.len(), 10, "same contours and segments");
        assert_eq!(winding(&path, Point::new(1.0, 1.0)), 1);
        assert_eq!(winding(&path, Point::new(5.0, 5.0)), 0);
        // Already counter-clockwise glyphs are left untouched.
        let before = path.clone();
        orient_glyph_contours(&mut path);
        assert_eq!(path, before);
    }

    #[test]
    fn reversed_curves_swap_their_control_points() {
        let p = Point::new;
        let mut path = Path::new();
        path.move_to(p(0.0, 0.0));
        path.curve_to(p(0.0, 5.0), p(5.0, 10.0), p(10.0, 10.0));
        path.line_to(p(10.0, 0.0));
        path.close();
        orient_glyph_contours(&mut path);
        assert_eq!(
            path.iter().collect::<Vec<_>>(),
            [
                PathElement::MoveTo(p(10.0, 0.0)),
                PathElement::LineTo(p(10.0, 10.0)),
                PathElement::CurveTo(p(5.0, 10.0), p(0.0, 5.0), p(0.0, 0.0)),
                PathElement::Close,
            ]
        );
    }

    /// `build_glyph_clip_path` produces a real outline clip + bbox from a
    /// loaded font (the geometry behind pattern-filled text). Uses the
    /// committed Quartz CFF fixture; skips gracefully if it is absent.
    #[test]
    fn glyph_clip_path_from_real_font() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../zpdf/tests/fixtures/quartz_cff_subset.pdf"
        );
        let Ok(data) = std::fs::read(fixture) else {
            return; // fixture not present in this checkout
        };
        let doc = zpdf_document::PdfDocument::open(data).expect("open fixture");
        // CMBX12 is object 4; GID 26 = 'U', GID 42 = 'n' both have outlines.
        let font =
            zpdf_document::font_loader::load_single_font(doc.file(), zpdf_core::ObjectId(4, 0))
                .expect("load CMBX12");
        let glyphs = vec![
            PositionedGlyph {
                glyph_id: 26,
                code_len: 0,
                code: 0,
                x: 0.0,
                y: 0.0,
                advance: 0.0,
            },
            PositionedGlyph {
                glyph_id: 42,
                code_len: 0,
                code: 0,
                x: 50.0,
                y: 0.0,
                advance: 0.0,
            },
        ];
        let (path, bounds) =
            build_glyph_clip_path(&glyphs, &Matrix::identity(), &font, 100.0, 1.0, &mut false)
                .expect("glyph clip path");
        assert!(!path.is_empty(), "clip path should carry outline elements");
        assert!(
            bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0,
            "non-empty bbox: {bounds:?}"
        );
        // Two ~100pt glyphs starting at x=0 and x=50 stay within a sane span.
        assert!(
            bounds.x0 >= -5.0 && bounds.x1 < 250.0,
            "bbox span: {bounds:?}"
        );
    }

    /// A FontCache with a single placeholder font registered as `F1`, so that a
    /// `/F1 Tf` selects an active font and text operators emit glyph runs.
    fn cache_with_f1() -> FontCache {
        let mut fc = FontCache::new();
        fc.insert(
            "F1".to_string(),
            zpdf_font::LoadedFont::new_placeholder("F1".to_string()),
        );
        fc
    }

    #[test]
    fn interpret_text_block() {
        let content = b"BT /F1 12 Tf 100 700 Td (Hello World) Tj ET";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut fc = cache_with_f1();
        let dl = ContentInterpreter::new(page_rect)
            .with_fonts(&mut fc)
            .interpret(content);
        assert_eq!(dl.commands.len(), 1);
        assert!(matches!(&dl.commands[0], RenderCommand::DrawGlyphRun(_)));
    }

    #[test]
    fn interpret_tj_array() {
        let content = b"BT /F1 12 Tf 100 700 Td [(AB) -200 (CD)] TJ ET";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut fc = cache_with_f1();
        let dl = ContentInterpreter::new(page_rect)
            .with_fonts(&mut fc)
            .interpret(content);
        assert_eq!(dl.commands.len(), 2); // two glyph runs
    }

    #[test]
    fn no_font_emits_no_glyph_run() {
        // Text shown without a selectable font must not emit a glyph run aliased
        // onto font id 0 (it would render with an unrelated font).
        let content = b"BT /F1 12 Tf 100 700 Td (Hello) Tj ET";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let dl = ContentInterpreter::new(page_rect).interpret(content);
        assert_eq!(dl.commands.len(), 0);
    }

    #[test]
    fn interpret_cmyk_color() {
        let content = b"0 0 0 1 k 100 100 50 50 re f";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let dl = ContentInterpreter::new(page_rect).interpret(content);
        assert_eq!(dl.commands.len(), 1);
        match &dl.commands[0] {
            RenderCommand::FillPath { paint, .. } => match paint {
                Paint::Solid(c) => {
                    // DeviceCMYK 100% K resolves through the Adobe polynomial to
                    // a dark near-black (~0.17, 0.18, 0.21), not pure black.
                    assert!((c.r - 0.171).abs() < 0.01, "r={}", c.r);
                    assert!((c.g - 0.182).abs() < 0.01, "g={}", c.g);
                    assert!((c.b - 0.206).abs() < 0.01, "b={}", c.b);
                }
                _ => panic!("expected solid paint"),
            },
            _ => panic!("expected fill path"),
        }
    }

    #[test]
    fn resolve_colorspace_names() {
        use zpdf_core::PdfName;
        use zpdf_image::ResolvedColorSpace as Rcs;
        let cs = PdfObject::Name(PdfName::new("DeviceCMYK"));
        assert_eq!(
            resolve_image_colorspace(
                None,
                None,
                Some(&cs),
                0,
                zpdf_color::RenderIntent::default()
            ),
            Some(Rcs::Cmyk)
        );
        let cs = PdfObject::Name(PdfName::new("CalGray"));
        assert!(matches!(
            resolve_image_colorspace(
                None,
                None,
                Some(&cs),
                0,
                zpdf_color::RenderIntent::default()
            ),
            Some(Rcs::CalGray(_))
        ));
        let cs = PdfObject::Name(PdfName::new("Pattern"));
        assert_eq!(
            resolve_image_colorspace(
                None,
                None,
                Some(&cs),
                0,
                zpdf_color::RenderIntent::default()
            ),
            None
        );
    }

    #[test]
    fn resolve_iccbased_n_components() {
        use zpdf_core::{PdfName, PdfStream};
        use zpdf_image::ResolvedColorSpace as Rcs;
        // [/ICCBased <stream /N 4>] — the profile stream is inline here; the
        // ref form takes the same path after one file.resolve().
        let mut profile_dict = zpdf_core::PdfDict::new();
        profile_dict.insert(PdfName::new("N"), PdfObject::Integer(4));
        let cs = PdfObject::Array(vec![
            PdfObject::Name(PdfName::new("ICCBased")),
            PdfObject::Stream(PdfStream::new(profile_dict, vec![])),
        ]);
        assert_eq!(
            resolve_image_colorspace(
                None,
                None,
                Some(&cs),
                0,
                zpdf_color::RenderIntent::default()
            ),
            Some(Rcs::Cmyk)
        );
    }

    #[test]
    fn resolve_indexed_with_inline_palette() {
        use zpdf_core::{PdfName, PdfString};
        use zpdf_image::ResolvedColorSpace as Rcs;
        let cs = PdfObject::Array(vec![
            PdfObject::Name(PdfName::new("Indexed")),
            PdfObject::Name(PdfName::new("DeviceRGB")),
            PdfObject::Integer(1),
            PdfObject::String(PdfString::new(vec![255, 0, 0, 0, 0, 255])),
        ]);
        match resolve_image_colorspace(
            None,
            None,
            Some(&cs),
            0,
            zpdf_color::RenderIntent::default(),
        ) {
            Some(Rcs::Indexed {
                base,
                hival,
                lookup,
            }) => {
                assert_eq!(*base, Rcs::Rgb);
                assert_eq!(hival, 1);
                assert_eq!(lookup, vec![255, 0, 0, 0, 0, 255]);
            }
            other => panic!("expected Indexed, got {other:?}"),
        }
    }

    /// Interpret `content` with `/CS0` bound to the inline colour space `cs`.
    /// A file holding `body` as object 1.
    fn single_object_file(body: &str) -> PdfFile {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let offset = pdf.len();
        pdf.extend_from_slice(format!("1 0 obj\n{body}\nendobj\n").as_bytes());
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n0 2\n0000000000 65535 f \n{offset:010} 00000 n \n\
                 trailer\n<< /Size 2 >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        PdfFile::parse(pdf).expect("single-object file")
    }

    #[test]
    fn disjoint_clips_stay_empty() {
        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        interp.intersect_clip_bounds(Some(Rect::new(0.0, 0.0, 10.0, 10.0)));
        interp.intersect_clip_bounds(Some(Rect::new(20.0, 20.0, 30.0, 30.0)));
        interp.intersect_clip_bounds(Some(Rect::new(0.0, 0.0, 100.0, 100.0)));
        let region = interp.shading_region();
        assert!(
            region.width() <= 0.0 || region.height() <= 0.0,
            "{region:?}"
        );
    }

    #[test]
    fn a_reselected_shading_pattern_is_built_once_per_base_ctm() {
        let file = single_object_file(
            "<< /PatternType 2 /Shading << /ShadingType 2 /ColorSpace /DeviceRGB \
             /Coords [0 0 100 0] /Function << /FunctionType 2 /Domain [0 1] \
             /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >> >>",
        );
        let mut resources = ResourceDict::default();
        resources.patterns.insert("P0".into(), ObjectId(1, 0));
        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &resources);
        let shading = |interp: &mut ContentInterpreter| match interp.resolve_pattern("P0") {
            (Some(PatternPaint::Shading(def)), _) => def,
            _ => panic!("shading pattern"),
        };
        let first = shading(&mut interp);
        assert!(std::sync::Arc::ptr_eq(&first, &shading(&mut interp)));

        // Under another base CTM (a form, a tile) the shading maps elsewhere.
        interp.base_ctm = Matrix::new(2.0, 0.0, 0.0, 2.0, 0.0, 0.0);
        let scaled = shading(&mut interp);
        assert!(!std::sync::Arc::ptr_eq(&first, &scaled));
        assert_eq!(scaled.to_page, interp.base_ctm);
    }

    fn fill_with_color_space(cs: PdfObject, content: &[u8]) -> DisplayList {
        let (file, _) = stencil_image_file();
        let mut resources = ResourceDict::default();
        resources.color_spaces_inline.insert("CS0".into(), cs);
        ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &resources)
            .interpret(content)
    }

    #[test]
    fn ext_gstate_dash_pattern_applies() {
        use zpdf_core::PdfName;
        let mut gs = PdfDict::new();
        gs.insert(
            PdfName::new("D"),
            PdfObject::Array(vec![
                PdfObject::Array(vec![PdfObject::Integer(3), PdfObject::Integer(2)]),
                PdfObject::Integer(1),
            ]),
        );
        let mut resources = ResourceDict::default();
        resources.ext_g_state_inline.insert("GS0".into(), gs);
        let (file, _) = stencil_image_file();
        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &resources)
            .interpret(b"/GS0 gs 0 0 m 50 0 l S");
        let dash = dl.commands.iter().find_map(|c| match c {
            RenderCommand::StrokePath { style, .. } => style.dash.clone(),
            _ => None,
        });
        let dash = dash.expect("dashed stroke");
        assert_eq!((dash.array, dash.phase), (vec![3.0, 2.0], 1.0));
    }

    #[test]
    fn stroke_pattern_region_covers_the_page_space_stroke() {
        // `10 0 0 10 0 0 cm 5 w`: a 50-unit stroke along y = 100.
        let mut path = Path::new();
        path.move_to(Point::new(100.0, 100.0));
        path.line_to(Point::new(200.0, 100.0));
        let style = StrokeStyle {
            width: 50.0,
            join: LineJoin::Round,
            ..StrokeStyle::default()
        };
        let bounds = ContentInterpreter::stroke_region_bounds(&path, &style).unwrap();
        assert!(bounds.y0 <= 75.0 && bounds.y1 >= 125.0, "{bounds:?}");
    }

    #[test]
    fn clipping_operators_apply_after_the_painting_operator() {
        let kinds = |content: &[u8]| -> Vec<&'static str> {
            ContentInterpreter::new(Rect::new(0.0, 0.0, 400.0, 400.0))
                .interpret(content)
                .commands
                .iter()
                .filter_map(|c| match c {
                    RenderCommand::PushClip { .. } => Some("clip"),
                    RenderCommand::PopClip => Some("pop"),
                    RenderCommand::StrokePath { .. } => Some("stroke"),
                    RenderCommand::FillPath { .. } => Some("fill"),
                    _ => None,
                })
                .collect()
        };
        // The stroke is painted under the previous (no) clip, not its own.
        assert_eq!(
            kinds(b"q 100 100 200 200 re W S 0 0 400 400 re f Q"),
            ["stroke", "clip", "fill", "pop"]
        );
        assert_eq!(
            kinds(b"q 100 100 200 200 re W* n 0 0 400 400 re f Q"),
            ["clip", "fill", "pop"]
        );
    }

    /// A pending `W` belongs to the stream that set it: the tiling cell its
    /// own `f` paints must not consume it, and a form ending on `re W` must
    /// not clip what follows the form.
    #[test]
    fn pending_clips_stay_in_their_own_content_stream() {
        // Bounds of the clips still open around the last fill.
        let clips_at_last_fill = |file: &PdfFile, resources: &ResourceDict, content: &[u8]| {
            let list = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
                .with_document(file, resources)
                .interpret(content);
            let mut open = Vec::new();
            let mut at_fill = Vec::new();
            for command in &list.commands {
                match command {
                    RenderCommand::PushClip { path, .. } => {
                        open.push(ContentInterpreter::path_bounds(path))
                    }
                    RenderCommand::PopClip => {
                        open.pop();
                    }
                    RenderCommand::FillPath { .. } => at_fill.clone_from(&open),
                    _ => {}
                }
            }
            at_fill
        };

        let pattern = single_object_file(
            "<< /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] \
             /XStep 10 /YStep 10 /Resources << >> /Length 11 >>\n\
             stream\n0 0 5 5 re f\nendstream",
        );
        let mut resources = ResourceDict::default();
        resources.patterns.insert("P0".into(), ObjectId(1, 0));
        assert_eq!(
            clips_at_last_fill(
                &pattern,
                &resources,
                b"/Pattern cs /P0 scn 0 0 50 50 re W f 0 g 0 0 100 100 re f",
            ),
            [Some(Rect::new(0.0, 0.0, 50.0, 50.0))]
        );

        let form = single_object_file(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] /Length 13 >>\n\
             stream\n0 0 10 10 re W\nendstream",
        );
        let mut resources = ResourceDict::default();
        resources.xobjects.insert("Fm0".into(), ObjectId(1, 0));
        assert_eq!(
            clips_at_last_fill(&form, &resources, b"/Fm0 Do 0 0 100 100 re f"),
            []
        );
    }

    #[test]
    fn self_referencing_color_space_names_terminate() {
        use zpdf_core::{PdfName, PdfString};
        // `/CS0 /CS0`, and an Indexed space whose base names its own resource.
        let cycle = PdfObject::Name(PdfName::new("CS0"));
        let indexed = PdfObject::Array(vec![
            PdfObject::Name(PdfName::new("Indexed")),
            PdfObject::Name(PdfName::new("CS0")),
            PdfObject::Integer(0),
            PdfObject::String(PdfString::new(vec![0, 0, 0])),
        ]);
        for cs in [cycle, indexed] {
            let dl = fill_with_color_space(cs, b"/CS0 cs 0 sc 0 0 10 10 re f");
            assert!(dl
                .commands
                .iter()
                .any(|c| matches!(c, RenderCommand::FillPath { .. })));
        }
    }

    #[test]
    fn lab_with_an_inverted_range_does_not_panic() {
        use zpdf_core::PdfName;
        let mut params = PdfDict::new();
        params.insert(
            PdfName::new("Range"),
            PdfObject::Array([10.0, -10.0, 100.0, -100.0].map(PdfObject::Real).to_vec()),
        );
        let lab = PdfObject::Array(vec![
            PdfObject::Name(PdfName::new("Lab")),
            PdfObject::Dict(params),
        ]);
        let dl = fill_with_color_space(lab, b"/CS0 cs 50 0 0 sc 0 0 10 10 re f");
        assert!(dl
            .commands
            .iter()
            .any(|c| matches!(c, RenderCommand::FillPath { .. })));
    }

    #[test]
    fn resolve_indexed_unresolvable_ref_without_file() {
        use zpdf_core::PdfName;
        // The palette hides behind a ref but there is no file to chase it:
        // resolution must fail (None) so zpdf-image falls back to inference.
        let cs = PdfObject::Array(vec![
            PdfObject::Name(PdfName::new("Indexed")),
            PdfObject::Name(PdfName::new("DeviceRGB")),
            PdfObject::Integer(1),
            PdfObject::Ref(ObjectId(7, 0)),
        ]);
        assert_eq!(
            resolve_image_colorspace(
                None,
                None,
                Some(&cs),
                0,
                zpdf_color::RenderIntent::default()
            ),
            None
        );
    }

    // ---- ICCBased with real profile transforms ----

    /// sRGB IEC61966-2.1 (built by littlecms), 3 components.
    const SRGB_ICC: &[u8] = include_bytes!("testdata/srgb.icc");

    /// `[/ICCBased <inline stream /N n>]` carrying `profile` as its data.
    fn iccbased_cs(profile: &[u8], n: i64) -> PdfObject {
        use zpdf_core::{PdfName, PdfStream};
        let mut profile_dict = zpdf_core::PdfDict::new();
        profile_dict.insert(PdfName::new("N"), PdfObject::Integer(n));
        PdfObject::Array(vec![
            PdfObject::Name(PdfName::new("ICCBased")),
            PdfObject::Stream(PdfStream::new(profile_dict, profile.to_vec())),
        ])
    }

    #[test]
    fn iccbased_with_profile_builds_transform() {
        let mut cache = zpdf_color::IccCache::new();
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut interp = ContentInterpreter::new(page_rect).with_colors(&mut cache);
        let acs = interp.resolve_color_space_obj(&iccbased_cs(SRGB_ICC, 3), 0);
        assert!(matches!(acs, ActiveColorSpace::Icc(_)), "got {acs:?}");
        assert_eq!(acs.components(), 3);
        // sRGB → sRGB is an identity: saturated red stays red.
        let c = interp.components_to_rgb(&acs, &[1.0, 0.0, 0.0]);
        assert!(c.r > 0.98 && c.g < 0.02 && c.b < 0.02, "got {c:?}");
    }

    #[test]
    fn iccbased_garbage_profile_falls_back_to_n() {
        let mut cache = zpdf_color::IccCache::new();
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut interp = ContentInterpreter::new(page_rect).with_colors(&mut cache);
        let acs = interp.resolve_color_space_obj(&iccbased_cs(&[0u8; 64], 3), 0);
        assert!(matches!(acs, ActiveColorSpace::DeviceRGB), "got {acs:?}");
        let acs = interp.resolve_color_space_obj(&iccbased_cs(b"junk", 4), 0);
        assert!(matches!(acs, ActiveColorSpace::DeviceCMYK), "got {acs:?}");
    }

    #[test]
    fn iccbased_n_mismatch_falls_back_to_n() {
        // A 3-channel profile declared with /N 4 contradicts the operand
        // count the content stream will supply: keep the /N fallback.
        let mut cache = zpdf_color::IccCache::new();
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut interp = ContentInterpreter::new(page_rect).with_colors(&mut cache);
        let acs = interp.resolve_color_space_obj(&iccbased_cs(SRGB_ICC, 4), 0);
        assert!(matches!(acs, ActiveColorSpace::DeviceCMYK), "got {acs:?}");
    }

    #[test]
    fn iccbased_without_cache_keeps_old_behavior() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut interp = ContentInterpreter::new(page_rect);
        let acs = interp.resolve_color_space_obj(&iccbased_cs(SRGB_ICC, 3), 0);
        assert!(matches!(acs, ActiveColorSpace::DeviceRGB), "got {acs:?}");
    }

    #[test]
    fn image_iccbased_with_profile_builds_transform() {
        use zpdf_image::ResolvedColorSpace as Rcs;
        let mut cache = zpdf_color::IccCache::new();
        let cs = iccbased_cs(SRGB_ICC, 3);
        match resolve_image_colorspace(
            None,
            Some(&mut cache),
            Some(&cs),
            0,
            zpdf_color::RenderIntent::default(),
        ) {
            Some(Rcs::Icc { ncomp: 3, .. }) => {}
            other => panic!("expected Icc, got {other:?}"),
        }
        // Garbage profile: back to the /N mapping.
        let cs = iccbased_cs(&[0u8; 64], 4);
        assert_eq!(
            resolve_image_colorspace(
                None,
                Some(&mut cache),
                Some(&cs),
                0,
                zpdf_color::RenderIntent::default()
            ),
            Some(Rcs::Cmyk)
        );
    }

    #[test]
    fn indexed_with_icc_base_bakes_palette() {
        use zpdf_core::{PdfName, PdfString};
        use zpdf_image::ResolvedColorSpace as Rcs;
        let mut cache = zpdf_color::IccCache::new();
        // Indexed over ICCBased sRGB: the palette converts ≈unchanged and the
        // base demotes to plain RGB.
        let cs = PdfObject::Array(vec![
            PdfObject::Name(PdfName::new("Indexed")),
            iccbased_cs(SRGB_ICC, 3),
            PdfObject::Integer(1),
            PdfObject::String(PdfString::new(vec![255, 0, 0, 0, 0, 255])),
        ]);
        match resolve_image_colorspace(
            None,
            Some(&mut cache),
            Some(&cs),
            0,
            zpdf_color::RenderIntent::default(),
        ) {
            Some(Rcs::Indexed { base, lookup, .. }) => {
                assert_eq!(*base, Rcs::Rgb);
                assert_eq!(lookup.len(), 6);
                assert!(
                    lookup[0] > 250 && lookup[1] < 5 && lookup[2] < 5,
                    "{lookup:?}"
                );
                assert!(
                    lookup[3] < 5 && lookup[4] < 5 && lookup[5] > 250,
                    "{lookup:?}"
                );
            }
            other => panic!("expected Indexed, got {other:?}"),
        }
    }

    // ---- Separation / DeviceN / NChannel colorant semantics ----

    fn nm(s: &str) -> PdfObject {
        PdfObject::Name(zpdf_core::PdfName::new(s))
    }

    /// A FunctionType 2 tint dict (`t → C1`). It parses to `None` without a file
    /// (the unit tests below exercise colorant *names* and `/Colorants`, never
    /// the transform itself), which the colour space tolerates.
    fn tint_fn() -> PdfObject {
        use zpdf_core::{PdfDict, PdfName};
        let mut d = PdfDict::new();
        d.insert(PdfName::new("FunctionType"), PdfObject::Integer(2));
        d.insert(
            PdfName::new("Domain"),
            PdfObject::Array(vec![PdfObject::Real(0.0), PdfObject::Real(1.0)]),
        );
        d.insert(PdfName::new("N"), PdfObject::Integer(1));
        PdfObject::Dict(d)
    }

    fn separation(name: &str) -> PdfObject {
        PdfObject::Array(vec![
            nm("Separation"),
            nm(name),
            nm("DeviceCMYK"),
            tint_fn(),
        ])
    }

    fn new_interp() -> ContentInterpreter<'static> {
        ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
    }

    #[test]
    fn separation_none_produces_no_marks() {
        let mut interp = new_interp();
        let acs = interp.resolve_color_space_obj(&separation("None"), 0);
        match &acs {
            ActiveColorSpace::Tint { names, .. } => assert_eq!(&names[..], &["None".to_string()]),
            other => panic!("expected Tint, got {other:?}"),
        }
        assert!(
            cs_produces_no_marks(&acs),
            "a /None Separation marks nothing"
        );
        // A normal spot still marks.
        let spot = interp.resolve_color_space_obj(&separation("PANTONE 032"), 0);
        assert!(!cs_produces_no_marks(&spot));
    }

    #[test]
    fn separation_all_is_overprint_knockout() {
        let mut interp = new_interp();
        let acs = interp.resolve_color_space_obj(&separation("All"), 0);
        // `All` paints every colorant (registration) → not selective overprint,
        // so it reports no colorant projection (a normal, knockout paint).
        assert!(
            interp.colorant_projection(&acs, &[1.0]).is_none(),
            "/All must knock out (no overprint mask)"
        );
        // A spot, by contrast, projects to its colorant.
        let spot = interp.resolve_color_space_obj(&separation("Spot"), 0);
        assert!(interp.colorant_projection(&spot, &[1.0]).is_some());
    }

    #[test]
    fn devicen_none_excluded_from_overprint_mask() {
        // [/DeviceN [/Cyan /None /Black] /DeviceCMYK fn] — Cyan and Black are
        // process colorants, the middle component is /None. The per-colorant
        // mask must take Cyan and Black and *drop* the None tint entirely.
        let cs = PdfObject::Array(vec![
            nm("DeviceN"),
            PdfObject::Array(vec![nm("Cyan"), nm("None"), nm("Black")]),
            nm("DeviceCMYK"),
            tint_fn(),
        ]);
        let mut interp = new_interp();
        let acs = interp.resolve_color_space_obj(&cs, 0);
        match &acs {
            ActiveColorSpace::Tint { colorants, .. } => {
                assert!(
                    colorants.is_some(),
                    "process+None DeviceN gets a per-colorant mask"
                );
            }
            other => panic!("expected Tint, got {other:?}"),
        }
        let c = interp
            .colorant_projection(&acs, &[0.5, 0.9, 0.2])
            .expect("colorant");
        // C=0.5, M=0, Y=0, K=0.2 — the None component's 0.9 contributes nothing.
        assert!((c.cmyk[0] - 0.5).abs() < 1e-6, "C {:?}", c.cmyk);
        assert_eq!(c.cmyk[1], 0.0, "M {:?}", c.cmyk);
        assert_eq!(c.cmyk[2], 0.0, "Y (None dropped) {:?}", c.cmyk);
        assert!((c.cmyk[3] - 0.2).abs() < 1e-6, "K {:?}", c.cmyk);
        assert!(!c.cmyk_device, "spot/DeviceN colorant is not OPM-governed");
    }

    #[test]
    fn plain_devicen_keeps_whole_transform_path() {
        // A plain DeviceN of two spots, no /Colorants and no /None, must NOT
        // switch to the per-colorant path — overprint stays byte-identical.
        let cs = PdfObject::Array(vec![
            nm("DeviceN"),
            PdfObject::Array(vec![nm("Spot1"), nm("Spot2")]),
            nm("DeviceCMYK"),
            tint_fn(),
        ]);
        let mut interp = new_interp();
        match interp.resolve_color_space_obj(&cs, 0) {
            ActiveColorSpace::Tint {
                colorants, names, ..
            } => {
                assert!(
                    colorants.is_none(),
                    "plain DeviceN keeps whole-transform overprint"
                );
                assert_eq!(names.len(), 2);
            }
            other => panic!("expected Tint, got {other:?}"),
        }
    }

    #[test]
    fn nchannel_colorants_classifies_spot() {
        // [/DeviceN [/Cyan /PANTONE] /DeviceCMYK fn << /Subtype /NChannel
        //   /Colorants << /PANTONE [/Separation /PANTONE /DeviceCMYK fn] >> >>]
        use zpdf_core::{PdfDict, PdfName};
        let mut colorants = PdfDict::new();
        colorants.insert(PdfName::new("PANTONE"), separation("PANTONE"));
        let mut attrs = PdfDict::new();
        attrs.insert(PdfName::new("Subtype"), nm("NChannel"));
        attrs.insert(PdfName::new("Colorants"), PdfObject::Dict(colorants));
        let cs = PdfObject::Array(vec![
            nm("DeviceN"),
            PdfObject::Array(vec![nm("Cyan"), nm("PANTONE")]),
            nm("DeviceCMYK"),
            tint_fn(),
            PdfObject::Dict(attrs),
        ]);
        let mut interp = new_interp();
        match interp.resolve_color_space_obj(&cs, 0) {
            ActiveColorSpace::Tint {
                colorants: Some(maps),
                ..
            } => {
                assert!(matches!(maps[0], ColorantMap::Process(0)), "Cyan→C");
                assert!(matches!(maps[1], ColorantMap::Spot(_)), "PANTONE→spot");
            }
            other => panic!("expected NChannel Tint with /Colorants, got {other:?}"),
        }
    }

    #[test]
    fn devicen_none_with_unclassifiable_spot_keeps_map() {
        // [/DeviceN [/Spot1 /None] /DeviceCMYK fn] with NO /Colorants: a /None
        // must still be dropped from the overprint mask, so the per-colorant map
        // is kept — the unclassifiable Spot1 isolated through the whole transform
        // (Whole), the /None dropped — rather than discarded (which would let the
        // /None tint leak into the whole-transform projection).
        let cs = PdfObject::Array(vec![
            nm("DeviceN"),
            PdfObject::Array(vec![nm("Spot1"), nm("None")]),
            nm("DeviceCMYK"),
            tint_fn(),
        ]);
        let mut interp = new_interp();
        match interp.resolve_color_space_obj(&cs, 0) {
            ActiveColorSpace::Tint {
                colorants: Some(maps),
                ..
            } => {
                assert!(
                    matches!(maps[0], ColorantMap::Whole),
                    "spot → Whole, got {:?}",
                    maps[0]
                );
                assert!(
                    matches!(maps[1], ColorantMap::None),
                    "None → None, got {:?}",
                    maps[1]
                );
            }
            other => panic!("expected per-colorant Tint, got {other:?}"),
        }
        // No /None to drop → an unclassifiable spot keeps the faithful whole-
        // transform path (colorants None), not a Whole-isolated approximation.
        let cs2 = PdfObject::Array(vec![
            nm("DeviceN"),
            PdfObject::Array(vec![nm("Spot1"), nm("Spot2")]),
            nm("DeviceCMYK"),
            tint_fn(),
        ]);
        match interp.resolve_color_space_obj(&cs2, 0) {
            ActiveColorSpace::Tint { colorants, .. } => assert!(colorants.is_none()),
            other => panic!("expected Tint, got {other:?}"),
        }
    }

    #[test]
    fn nchannel_missing_colorant_falls_back() {
        // A spot with no /Colorants entry cannot be isolated → no per-colorant
        // mask (whole-transform path), so overprint never regresses.
        use zpdf_core::{PdfDict, PdfName};
        let mut attrs = PdfDict::new();
        attrs.insert(PdfName::new("Subtype"), nm("NChannel"));
        attrs.insert(PdfName::new("Colorants"), PdfObject::Dict(PdfDict::new()));
        let cs = PdfObject::Array(vec![
            nm("DeviceN"),
            PdfObject::Array(vec![nm("Cyan"), nm("UnknownSpot")]),
            nm("DeviceCMYK"),
            tint_fn(),
            PdfObject::Dict(attrs),
        ]);
        let mut interp = new_interp();
        match interp.resolve_color_space_obj(&cs, 0) {
            ActiveColorSpace::Tint { colorants, .. } => {
                assert!(
                    colorants.is_none(),
                    "unmapped spot disables the per-colorant mask"
                );
            }
            other => panic!("expected Tint, got {other:?}"),
        }
    }

    #[test]
    fn graphics_state_limit_truncates_before_unbounded_clones() {
        let page = Rect::new(0.0, 0.0, 10.0, 10.0);
        let dl = ContentInterpreter::new(page)
            .with_graphics_state_limit(2)
            .interpret(b"q q q 0 0 1 1 re f");
        assert!(!dl
            .commands
            .iter()
            .any(|c| matches!(c, RenderCommand::FillPath { .. })));

        let dl = ContentInterpreter::new(page)
            .with_graphics_state_limit(2)
            .interpret(b"q q 0 0 1 1 re f Q Q");
        assert!(dl
            .commands
            .iter()
            .any(|c| matches!(c, RenderCommand::FillPath { .. })));
    }

    #[test]
    fn marked_content_limit_truncates_before_unbounded_stack_growth() {
        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
            .with_marked_content_limit(2)
            .interpret(b"/Span BMC /Span BMC /Span BMC 0 0 1 1 re f");
        assert!(!dl
            .commands
            .iter()
            .any(|c| matches!(c, RenderCommand::FillPath { .. })));
    }

    #[test]
    fn parse_limits_configure_all_content_budgets() {
        let limits = ParseLimits {
            max_page_operators: 7,
            max_operand_stack_depth: 8,
            max_graphics_state_depth: 9,
            max_marked_content_depth: 10,
            max_image_cache_bytes: 11,
            max_font_cache_bytes: 12,
            max_image_pixels: 13,
            ..ParseLimits::default()
        };
        let interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0)).with_limits(&limits);
        assert_eq!(interp.max_ops, 7);
        assert_eq!(interp.max_operand_stack_depth, 8);
        assert_eq!(interp.max_graphics_state_depth, 9);
        assert_eq!(interp.max_marked_content_depth, 10);
        assert_eq!(interp.max_image_cache_bytes, 11);
        assert_eq!(interp.max_font_cache_bytes, 12);
        assert_eq!(interp.parse_limits.max_image_pixels, 13);
    }

    #[test]
    fn inline_images_honor_custom_pixel_limit() {
        let limits = ParseLimits {
            max_image_pixels: 3,
            ..ParseLimits::default()
        };
        let mut dict = PdfDict::new();
        dict.insert(zpdf_core::PdfName::new("W"), PdfObject::Integer(2));
        dict.insert(zpdf_core::PdfName::new("H"), PdfObject::Integer(2));
        dict.insert(zpdf_core::PdfName::new("BPC"), PdfObject::Integer(8));
        dict.insert(
            zpdf_core::PdfName::new("CS"),
            PdfObject::Name(zpdf_core::PdfName::new("G")),
        );

        let mut cache = ImageCache::new();
        {
            let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
                .with_images(&mut cache)
                .with_limits(&limits);
            interp.do_inline_image(dict, vec![0; 4]);
            assert!(!interp
                .display_list
                .commands
                .iter()
                .any(|command| matches!(command, RenderCommand::DrawImage(_))));
        }
        assert!(cache.is_empty());
    }

    #[test]
    fn image_cache_limit_drops_draw_without_retaining_pixels() {
        let mut cache = ImageCache::new();
        {
            let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
                .with_images(&mut cache)
                .with_image_cache_limit(3);
            interp.emit_draw_image(zpdf_image::DecodedImage {
                width: 1,
                height: 1,
                data: vec![0, 0, 0, 255],
                has_alpha: false,
                is_image_mask: false,
                premultiplied: false,
            });
            assert!(!interp
                .display_list
                .commands
                .iter()
                .any(|command| matches!(command, RenderCommand::DrawImage(_))));
        }
        assert!(cache.is_empty());
        assert_eq!(cache.bytes_used(), 0);
    }

    #[test]
    fn non_finite_and_overflowing_transforms_are_ignored() {
        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        interp.operand_stack.push(PdfObject::Real(f64::INFINITY));
        assert_eq!(interp.pop_f64(), 0.0);

        let scale = |interp: &mut ContentInterpreter<'_>| {
            interp.operand_stack.extend([
                PdfObject::Real(1.0e308),
                PdfObject::Real(0.0),
                PdfObject::Real(0.0),
                PdfObject::Real(1.0e308),
                PdfObject::Real(0.0),
                PdfObject::Real(0.0),
            ]);
            interp.execute_operator("cm");
            interp.operand_stack.clear();
        };
        scale(&mut interp);
        scale(&mut interp); // would overflow without the post-concat guard
        assert!(interp.current.ctm.is_finite());
    }

    // -- Interpret-stage timings (`InterpretStats`) -------------------------

    /// The wrapper split (`interpret` = `interpret_with_stats().0`) must not
    /// change the emitted display list — otherwise every measurement taken with
    /// stats on would describe a different page than production renders.
    #[test]
    fn interpret_with_stats_emits_the_same_display_list() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        for content in [
            &b"1 0 0 rg 100 200 300 400 re f"[..],
            &b"q 0.5 g 100 100 50 50 re f Q 10 10 20 20 re f"[..],
            &b"0 0 1 RG 2 w 0 0 m 100 100 l S"[..],
        ] {
            let plain = ContentInterpreter::new(page_rect).interpret(content);
            let (measured, _) = ContentInterpreter::new(page_rect).interpret_with_stats(content);
            assert_eq!(
                plain.commands.len(),
                measured.commands.len(),
                "command count must not depend on whether stats are collected"
            );
        }
    }

    #[test]
    fn interpret_stats_stamp_total_time() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let (_, stats) =
            ContentInterpreter::new(page_rect).interpret_with_stats(b"100 200 300 400 re f");
        assert_eq!(stats.shading_rasters, 0, "this page has no shading");
        assert_eq!(stats.shading_ns, 0, "no shading work to attribute");
        // Wall-clock: bare wasm32 has no clock (`elapsed()` is defined as zero).
        #[cfg(not(target_arch = "wasm32"))]
        assert!(stats.total_ns > 0, "total_ns must bracket the interpret");
        #[cfg(target_arch = "wasm32")]
        assert!(stats.is_empty() || stats.total_ns == 0);
    }

    // Two complete one-pixel inline images; each BI ... EI is one operation.
    const INLINE_IMAGE_PAIR: &str =
        "BI /W 1 /H 1 /BPC 8 /CS /G ID x EI BI /W 1 /H 1 /BPC 8 /CS /G ID y EI";

    #[test]
    fn inline_images_count_toward_the_operator_limit() {
        let page = Rect::new(0.0, 0.0, 10.0, 10.0);
        for render_images in [false, true] {
            for max_ops in [1, 2] {
                let limits = ParseLimits {
                    max_page_operators: max_ops,
                    ..ParseLimits::default()
                };
                let mut cache = ImageCache::new();
                let interp = ContentInterpreter::new(page).with_limits(&limits);
                let interp = if render_images {
                    interp.with_images(&mut cache)
                } else {
                    interp
                };
                let (dl, stats) = interp.interpret_with_stats(INLINE_IMAGE_PAIR.as_bytes());
                assert_eq!(stats.operators_executed, max_ops);
                assert_eq!(stats.diagnostics.resource_limit_reached, max_ops == 1);
                assert_eq!(
                    dl.commands
                        .iter()
                        .filter(|cmd| matches!(cmd, RenderCommand::DrawImage(_)))
                        .count(),
                    if render_images { max_ops as usize } else { 0 },
                );
                if max_ops == 2 {
                    assert!(stats.diagnostics.is_complete());
                }
            }
        }
    }

    #[test]
    fn inline_images_in_forms_share_the_operator_limit() {
        let form = single_object_file(&format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] /Length {} >>\n\
             stream\n{INLINE_IMAGE_PAIR}\nendstream",
            INLINE_IMAGE_PAIR.len(),
        ));
        let mut resources = ResourceDict::default();
        resources.xobjects.insert("Fm0".into(), ObjectId(1, 0));
        for max_ops in [2, 3] {
            let limits = ParseLimits {
                max_page_operators: max_ops,
                ..ParseLimits::default()
            };
            let mut cache = ImageCache::new();
            let (dl, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
                .with_document(&form, &resources)
                .with_images(&mut cache)
                .with_limits(&limits)
                .interpret_with_stats(b"/Fm0 Do");
            // The invoking Do and each inline image consume one operation.
            assert_eq!(stats.operators_executed, max_ops);
            assert_eq!(stats.diagnostics.resource_limit_reached, max_ops == 2);
            assert_eq!(
                dl.commands
                    .iter()
                    .filter(|cmd| matches!(cmd, RenderCommand::DrawImage(_)))
                    .count(),
                (max_ops - 1) as usize,
            );
            if max_ops == 3 {
                assert!(stats.diagnostics.is_complete());
            }
        }
    }

    #[test]
    fn inline_images_in_patterns_share_the_operator_limit() {
        let pattern = single_object_file(&format!(
            "<< /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] \
             /XStep 10 /YStep 10 /Resources << >> /Length {} >>\n\
             stream\n{INLINE_IMAGE_PAIR}\nendstream",
            INLINE_IMAGE_PAIR.len(),
        ));
        let mut resources = ResourceDict::default();
        resources.patterns.insert("P0".into(), ObjectId(1, 0));
        for max_ops in [5, 100] {
            let limits = ParseLimits {
                max_page_operators: max_ops,
                ..ParseLimits::default()
            };
            let mut cache = ImageCache::new();
            let (dl, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
                .with_document(&pattern, &resources)
                .with_images(&mut cache)
                .with_limits(&limits)
                .interpret_with_stats(b"/Pattern cs /P0 scn 0 0 1 1 re f");
            let images: usize = dl
                .commands
                .iter()
                .map(|cmd| match cmd {
                    RenderCommand::FillPath {
                        paint: Paint::Tiling(tiling),
                        ..
                    } => tiling
                        .commands
                        .commands
                        .iter()
                        .filter(|c| matches!(c, RenderCommand::DrawImage(_)))
                        .count(),
                    _ => 0,
                })
                .sum();
            // cs, scn, re, f plus every inline image inside the single pre-interpreted cell.
            assert_eq!(stats.operators_executed, 4 + images as u64);
            if max_ops == 5 {
                assert_eq!(images, 1);
                assert!(stats.diagnostics.resource_limit_reached);
            } else {
                assert_eq!(
                    images, 2,
                    "the pattern cell must execute both inline images"
                );
                assert!(stats.diagnostics.is_complete());
            }
        }
    }

    #[test]
    fn operator_count_includes_unattributed_work_and_stops_at_the_limit() {
        let page = Rect::new(0.0, 0.0, 10.0, 10.0);
        let (_, complete) = ContentInterpreter::new(page).interpret_with_stats(b"q Q q Q");
        assert_eq!(complete.operators_executed, 4);
        assert!(complete.diagnostics.is_complete());
        let limits = ParseLimits {
            max_page_operators: 3,
            ..ParseLimits::default()
        };
        let (_, partial) = ContentInterpreter::new(page)
            .with_limits(&limits)
            .interpret_with_stats(b"q Q q Q");
        assert_eq!(partial.operators_executed, 3);
        assert!(partial.diagnostics.resource_limit_reached);
    }

    /// The work buckets are opt-in: with no flag, an instrumented operator must
    /// cost nothing and report nothing, while the always-on timings still work.
    #[test]
    fn work_buckets_are_off_unless_requested() {
        let (_, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 612.0, 792.0))
            .interpret_with_stats(b"1 0 0 rg 100 200 300 400 re f 0 0 m 10 10 l S");
        for (name, ns) in [
            ("path", stats.path_ns),
            ("paint", stats.paint_ns),
            ("text", stats.text_ns),
            ("image", stats.image_ns),
            ("form", stats.form_ns),
            ("color", stats.color_ns),
        ] {
            assert_eq!(ns, 0, "{name} bucket must stay empty without the flag");
        }
        assert_eq!(
            stats.path_ops
                + stats.paint_ops
                + stats.text_ops
                + stats.image_ops
                + stats.form_ops
                + stats.color_ops,
            0,
            "counters are part of the opt-in collection too"
        );
    }

    /// With collection requested, each kind of operator lands in its own bucket
    /// and the counters agree with what the content actually did.
    #[test]
    fn work_buckets_attribute_each_kind_of_operator() {
        let (dl, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 612.0, 792.0))
            .with_work_timing(true)
            .interpret_with_stats(b"1 0 0 rg 100 200 300 400 re f 0 0 m 10 10 l S");

        assert_eq!(dl.commands.len(), 2, "one fill, one stroke");
        assert_eq!(stats.color_ops, 1, "one `rg`");
        assert_eq!(stats.path_ops, 3, "`re` + `m` + `l`");
        assert_eq!(stats.paint_ops, 2, "`f` + `S`");
        assert_eq!(
            stats.text_ops + stats.image_ops + stats.form_ops,
            0,
            "this content has no text, image or form work"
        );

        // Wall-clock buckets say nothing on a platform without a clock.
        #[cfg(not(target_arch = "wasm32"))]
        {
            assert!(
                stats.paint_ns > 0,
                "paint work must be attributed: {stats:?}"
            );
            let attributed = stats.path_ns
                + stats.paint_ns
                + stats.text_ns
                + stats.image_ns
                + stats.form_ns
                + stats.color_ns;
            assert!(
                attributed <= stats.total_ns,
                "attribution {attributed} must not exceed the total {}",
                stats.total_ns
            );
        }
    }

    /// An instrumented run must emit exactly what production emits: these
    /// numbers are read as a description of the page, so a divergence in the
    /// output would be a rendering bug, not a measurement artefact.
    #[test]
    fn work_timing_does_not_change_the_display_list() {
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        for content in [
            &b"1 0 0 rg 100 200 300 400 re f"[..],
            &b"q 0.5 g 100 100 50 50 re f Q 10 10 20 20 re f"[..],
            &b"0 0 1 RG 2 w 0 0 m 100 100 l S"[..],
        ] {
            let plain = ContentInterpreter::new(page_rect).interpret(content);
            let (measured, _) = ContentInterpreter::new(page_rect)
                .with_work_timing(true)
                .interpret_with_stats(content);
            assert_eq!(plain.commands.len(), measured.commands.len());
        }
    }

    /// A shading's cost is only observable in this stage — the backends receive
    /// it as a plain image. Assert it is actually attributed.
    #[test]
    fn emit_shading_image_attributes_shading_cost() {
        use crate::shading::{MeshTriangle, MeshVertex, ShadingDef, ShadingKind};

        let def = ShadingDef {
            kind: ShadingKind::Mesh {
                triangles: vec![MeshTriangle {
                    v: [
                        MeshVertex {
                            x: 0.0,
                            y: 0.0,
                            rgb: [1.0, 0.0, 0.0],
                        },
                        MeshVertex {
                            x: 1.0,
                            y: 0.0,
                            rgb: [0.0, 1.0, 0.0],
                        },
                        MeshVertex {
                            x: 0.0,
                            y: 1.0,
                            rgb: [0.0, 0.0, 1.0],
                        },
                    ],
                }],
            },
            lut: vec![[0.0, 0.0, 0.0], [1.0, 1.0, 1.0]],
            extend_start: false,
            extend_end: false,
            to_page: Matrix::identity(),
        };

        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        interp.emit_shading_image(&def, Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(
            interp.stats.shading_rasters, 1,
            "one mesh shading must be recorded"
        );
        #[cfg(not(target_arch = "wasm32"))]
        assert!(
            interp.stats.shading_ns > 0,
            "shading time must be attributed"
        );
    }

    fn clip_counts_and_balance(dl: &DisplayList) -> (usize, usize, i32) {
        let mut pushes = 0usize;
        let mut pops = 0usize;
        let mut depth = 0i32;
        let mut min_depth = 0i32;
        for cmd in &dl.commands {
            match cmd {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    pushes += 1;
                    depth += 1;
                }
                RenderCommand::PopClip => {
                    pops += 1;
                    depth -= 1;
                    min_depth = min_depth.min(depth);
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0, "unbalanced PushClip/PopClip: {dl:?}");
        assert!(min_depth >= 0, "PopClip underflow: {dl:?}");
        (pushes, pops, min_depth)
    }

    #[test]
    fn covering_rectangle_clip_is_elided_and_smaller_is_kept() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);

        // Without a render scale, scale-independent display lists retain clips.
        let dl_no_scale = ContentInterpreter::new(page)
            .interpret(b"q 0 0 200 200 re W n 0 0 200 200 re W n 10 10 20 20 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_no_scale), (2, 2, 0));

        // The first (only) clip on an unclipped page is kept so tiny-skia stays
        // on its masked SourceOver compositing arithmetic.
        let dl_only_mask = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 0 0 200 200 re W n 10 10 20 20 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_only_mask), (1, 1, 0));

        // Under an active outer mask, exact page rect, oversized rect,
        // negative-extent rect, and manually constructed rects (both W and W*)
        // are all elided at scale 1.0 where the raster is exactly 200x200.
        for inner in [
            &b"0 0 200 200 re W n"[..],
            &b"-10 -10 220 220 re W* n"[..],
            &b"200 200 -200 -200 re W n"[..],
            &b"0 0 m 200 0 l 200 200 l 0 200 l h W n"[..],
            &b"0 0 m 200 0 l 200 200 l 0 200 l 0 0 l h W* n"[..],
            &b"0 0 m 0 200 l 200 200 l 200 0 l W n"[..],
        ] {
            let mut content = b"q 0 0 200 200 re W n ".to_vec();
            content.extend_from_slice(inner);
            content.extend_from_slice(b" 10 10 20 20 re f Q");
            let dl = ContentInterpreter::new(page)
                .with_clip_elision_scale(1.0)
                .interpret(&content);
            let (pushes, pops, _) = clip_counts_and_balance(&dl);
            assert_eq!(
                (pushes, pops),
                (1, 1),
                "expected nested covering clip elided for {:?}",
                String::from_utf8_lossy(inner)
            );
            assert_eq!(
                dl.commands
                    .iter()
                    .filter(|c| matches!(c, RenderCommand::FillPath { .. }))
                    .count(),
                1
            );
        }

        // Smaller on any single edge (left, bottom, right, top) must be kept.
        for inner in [
            &b"0.5 0 199.5 200 re W n"[..],
            &b"0 0.5 200 199.5 re W n"[..],
            &b"0 0 199.5 200 re W n"[..],
            &b"0 0 200 199.5 re W* n"[..],
            &b"10 10 180 180 re W n"[..],
        ] {
            let mut content = b"q 0 0 200 200 re W n ".to_vec();
            content.extend_from_slice(inner);
            content.extend_from_slice(b" 0 0 200 200 re f Q");
            let dl = ContentInterpreter::new(page)
                .with_clip_elision_scale(1.0)
                .interpret(&content);
            let (pushes, pops, _) = clip_counts_and_balance(&dl);
            assert_eq!(
                (pushes, pops),
                (2, 2),
                "expected narrower clip kept for {:?}",
                String::from_utf8_lossy(inner)
            );
        }

        // Fractional page edge (100.5 pt -> 101 columns at scale 1) and
        // fractional zoom (200 pt * 150/72 = 416.67 -> 417 columns) keep a
        // nested page-sized clip, while a nested clip covering the full ceil
        // raster elides.
        let frac_page = Rect::new(0.0, 0.0, 100.5, 100.0);
        let dl_frac_kept = ContentInterpreter::new(frac_page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 0 0 101 100 re W n 0 0 100.5 100 re W n 0 0 120 100 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_frac_kept), (2, 2, 0));
        let dl_frac_elided = ContentInterpreter::new(frac_page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 0 0 101 100 re W n 0 0 101 100 re W n 0 0 120 100 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_frac_elided), (1, 1, 0));

        let dl_zoom_kept = ContentInterpreter::new(page)
            .with_clip_elision_scale(150.0 / 72.0)
            .interpret(b"q -10 -10 220 220 re W n 0 0 200 200 re W n -10 -10 220 220 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_zoom_kept), (2, 2, 0));
        let dl_zoom_elided = ContentInterpreter::new(page)
            .with_clip_elision_scale(150.0 / 72.0)
            .interpret(b"q -10 -10 220 220 re W n -10 -10 220 220 re W n -10 -10 220 220 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_zoom_elided), (1, 1, 0));

        // Non-zero page origin: nested clip must cover [50, 100, 250, 400].
        let shifted = Rect::new(50.0, 100.0, 250.0, 400.0);
        let dl_elided = ContentInterpreter::new(shifted)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 50 100 200 300 re W n 50 100 200 300 re W n 60 110 10 10 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_elided), (1, 1, 0));
        let dl_kept = ContentInterpreter::new(shifted)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 50 100 200 300 re W n 0 0 200 300 re W n 60 110 10 10 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_kept), (2, 2, 0));
    }

    #[test]
    fn nested_clip_covering_narrowed_bounds_is_elided() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);
        // First clip narrows to [40, 40, 140, 140] (exact integer px at scale 1).
        // Second [20, 20, 160, 160] and third [40, 40, 140, 140] cover all
        // affected device pixels (elided). Fourth [50, 50, 130, 130] narrows
        // further (kept).
        let dl = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(
                b"q 40 40 100 100 re W n \
                  q 20 20 140 140 re W n \
                  q 40 40 100 100 re W* n \
                  q 50 50 80 80 re W n \
                  0 0 200 200 re f \
                  Q Q Q Q",
            );
        assert_eq!(clip_counts_and_balance(&dl), (2, 2, 0));

        // Nested clips sharing a fractional antialiased edge (x = 10.5 at scale 1)
        // or two distinct edges in the same pixel column (10.5 and 10.2) must
        // both be kept; covering the entire column (x = 10.0..61.0) elides.
        let dl_shared_aa = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 10.5 10.5 50 50 re W n 10.5 10.5 50 50 re W n 0 0 200 200 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_shared_aa), (2, 2, 0));

        let dl_same_pixel = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 10.5 10.5 50 50 re W n 10.2 10.0 51.0 51.0 re W n 0 0 200 200 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_same_pixel), (2, 2, 0));

        let dl_covers_aa_px = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 10.5 10.5 50 50 re W n 10.0 10.0 51.0 51.0 re W n 0 0 200 200 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_covers_aa_px), (1, 1, 0));

        // A non-rectangular clip (triangle inside [40, 40, 140, 140]) also
        // narrows clip_elision_bounds, so a subsequent covering rect is elided.
        let dl_tri = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(
                b"q 40 40 m 140 40 l 90 140 l h W n \
                  30 30 120 120 re W n \
                  0 0 200 200 re f Q",
            );
        assert_eq!(clip_counts_and_balance(&dl_tri), (1, 1, 0));
    }

    #[test]
    fn rotated_skewed_and_bowtie_clips_are_not_falsely_elided() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);

        for content in [
            // 45-degree rotated oversized rectangle under an active clip.
            &b"q 0 0 200 200 re W n 0.7071 0.7071 -0.7071 0.7071 100 -50 cm 0 0 300 300 re W n 0 0 200 200 re f Q"[..],
            // Slight (~1 deg) rotation.
            &b"q 0 0 200 200 re W n 0.9998 0.0175 -0.0175 0.9998 0 0 cm -50 -50 300 300 re W n 0 0 200 200 re f Q"[..],
            // Skewed parallelogram.
            &b"q 0 0 200 200 re W n 1 0.1 0 1 0 0 cm -50 -50 300 300 re W n 0 0 200 200 re f Q"[..],
            // Self-intersecting bowtie touching all four page corners.
            &b"q 0 0 200 200 re W n 0 0 m 200 200 l 200 0 l 0 200 l h W n 0 0 200 200 re f Q"[..],
        ] {
            let dl = ContentInterpreter::new(page)
                .with_clip_elision_scale(1.0)
                .interpret(content);
            assert_eq!(
                clip_counts_and_balance(&dl),
                (2, 2, 0),
                "non-axis-aligned clip must not be elided: {:?}",
                String::from_utf8_lossy(content)
            );
        }

        // Exact 90-degree page rotation maps [0, 0, 200, 100] cleanly onto the
        // rotated [0, 0, 100, 200] page_rect, so a nested full-page rect is
        // elided while a half-page rect is kept.
        let rect = Rect::new(0.0, 0.0, 200.0, 100.0);
        let dl_rot_cover = ContentInterpreter::new(rect)
            .with_clip_elision_scale(1.0)
            .with_page_rotation(90)
            .interpret(b"q 0 0 200 100 re W n 0 0 200 100 re W n 0 0 10 10 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_rot_cover), (1, 1, 0));

        let dl_rot_small = ContentInterpreter::new(rect)
            .with_clip_elision_scale(1.0)
            .with_page_rotation(90)
            .interpret(b"q 0 0 200 100 re W n 0 0 100 100 re W n 0 0 10 10 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_rot_small), (2, 2, 0));
    }

    #[test]
    fn multi_subpath_degenerate_nonfinite_and_unbounded_clips_are_kept() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);

        for content in [
            // Outer covering rect with an inner hole punched by W* (even-odd).
            &b"q 0 0 200 200 re W n 0 0 200 200 re 50 50 100 100 re W* n 0 0 200 200 re f Q"[..],
            // Duplicate covering rect with W* cancels to an empty clip.
            &b"q 0 0 200 200 re W n 0 0 200 200 re 0 0 200 200 re W* n 0 0 200 200 re f Q"[..],
            // Empty clip path hides all subsequent drawing.
            &b"q 0 0 200 200 re W n W n 0 0 200 200 re f Q"[..],
            // Zero-width and zero-height degenerate rects hide all drawing.
            &b"q 0 0 200 200 re W n 0 0 0 200 re W n 0 0 200 200 re f Q"[..],
            &b"q 0 0 200 200 re W n 0 0 200 0 re W n 0 0 200 200 re f Q"[..],
        ] {
            let dl = ContentInterpreter::new(page)
                .with_clip_elision_scale(1.0)
                .interpret(content);
            assert_eq!(
                clip_counts_and_balance(&dl),
                (2, 2, 0),
                "clip must not be elided: {:?}",
                String::from_utf8_lossy(content)
            );
        }

        // Non-finite coordinates never qualify as an axis-aligned rect or finite bounds.
        let mut nan_path = Path::new();
        nan_path.rect(Rect::new(0.0, 0.0, f64::NAN, 200.0));
        assert_eq!(ContentInterpreter::axis_aligned_rect(&nan_path), None);
        assert_eq!(ContentInterpreter::path_bounds(&nan_path), None);

        let mut inf_path = Path::new();
        inf_path.rect(Rect::new(0.0, 0.0, f64::INFINITY, 200.0));
        assert_eq!(ContentInterpreter::axis_aligned_rect(&inf_path), None);
        assert_eq!(ContentInterpreter::path_bounds(&inf_path), None);

        // Unbounded or degenerate initial page_rect prevents elision.
        for bad_page in [
            Rect::new(0.0, 0.0, f64::INFINITY, 200.0),
            Rect::new(0.0, 0.0, 0.0, 0.0),
        ] {
            let dl = ContentInterpreter::new(bad_page)
                .with_clip_elision_scale(1.0)
                .interpret(b"q 0 0 200 200 re W n 0 0 200 200 re W n 0 0 200 200 re f Q");
            assert_eq!(clip_counts_and_balance(&dl), (2, 2, 0));
        }
    }

    #[test]
    fn q_restore_clip_depth_stays_balanced_with_mixed_elided_and_kept_clips() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);

        // Mix elided and kept clips across sibling/nested q..Q scopes and
        // unclosed q levels at EOF.
        let dl = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(
                b"q 0 0 200 200 re W n 10 10 100 100 re W n 0 0 200 200 re W n \
                  q 5 5 120 120 re W n 20 20 50 50 re W n 15 15 60 60 re W n \
                  0 0 200 200 re f Q \
                  0 0 200 200 re f Q \
                  q 0 0 200 200 re W n 0 0 200 200 re W n 0 0 200 200 re f Q \
                  q 30 30 40 40 re W n 0 0 200 200 re W n 0 0 200 200 re f",
            );
        // Kept clips: scope 1 initial [0,0,200,200] + [10,10,110,110] + nested
        // [20,20,70,70], scope 2 initial [0,0,200,200], and unclosed
        // [30,30,70,70]. Five nested covering clips are elided.
        assert_eq!(clip_counts_and_balance(&dl), (5, 5, 0));

        // Unbalanced Q at page bottom pops the active clip and clears
        // clip_elision_bounds so a later [20,20,140,140] clip (which covered the
        // popped [50,50,100,100] clip) is NOT falsely elided.
        let dl_unbalanced = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(b"50 50 50 50 re W n Q 20 20 120 120 re W n 0 0 200 200 re f");
        assert_eq!(clip_counts_and_balance(&dl_unbalanced), (2, 2, 0));
    }

    #[test]
    fn form_bbox_elision_and_soft_mask_clip_isolation() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);

        // Full-page Form /BBox [0 0 200 200] keeps its only clip when unclipped,
        // elides under an active page clip, and is kept under a 45-degree CTM.
        let full_form = single_object_file(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 200 200] /Length 15 >>\n\
             stream\n10 10 20 20 re f\nendstream",
        );
        let mut resources = ResourceDict::default();
        resources.xobjects.insert("FmFull".into(), ObjectId(1, 0));
        let dl_unclipped = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .with_document(&full_form, &resources)
            .interpret(b"/FmFull Do");
        assert_eq!(clip_counts_and_balance(&dl_unclipped), (1, 1, 0));

        let dl_full = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .with_document(&full_form, &resources)
            .interpret(b"q 0 0 200 200 re W n /FmFull Do Q");
        assert_eq!(clip_counts_and_balance(&dl_full), (1, 1, 0));

        let dl_rot = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .with_document(&full_form, &resources)
            .interpret(b"q 0 0 200 200 re W n 0.7071 0.7071 -0.7071 0.7071 0 0 cm /FmFull Do Q");
        assert_eq!(clip_counts_and_balance(&dl_rot), (2, 2, 0));

        // Smaller Form /BBox [20 20 120 120] is kept even under an active page clip.
        let small_form = single_object_file(
            "<< /Type /XObject /Subtype /Form /BBox [20 20 120 120] /Length 15 >>\n\
             stream\n30 30 20 20 re f\nendstream",
        );
        let dl_small = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .with_document(&small_form, &resources)
            .interpret(b"q 0 0 200 200 re W n /FmFull Do Q");
        assert_eq!(clip_counts_and_balance(&dl_small), (2, 2, 0));

        // Soft mask /G is interpreted into a detached DisplayList that starts
        // unclipped: an outer [10, 10, 20, 20] clip on the page must not cause
        // a [0, 0, 100, 100] clip inside /G to be elided.
        let smask_form = single_object_file(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 200 200] \
             /Group << /S /Transparency /CS /DeviceGray >> /Length 36 >>\n\
             stream\n0 0 100 100 re W n 1 g 0 0 200 200 re f\nendstream",
        );
        let mut gs = PdfDict::new();
        let mut sm = PdfDict::new();
        sm.insert(
            zpdf_core::PdfName::new("S"),
            PdfObject::Name(zpdf_core::PdfName::new("Luminosity")),
        );
        sm.insert(zpdf_core::PdfName::new("G"), PdfObject::Ref(ObjectId(1, 0)));
        gs.insert(zpdf_core::PdfName::new("SMask"), PdfObject::Dict(sm));
        let mut sm_res = ResourceDict::default();
        sm_res.ext_g_state_inline.insert("GS0".into(), gs);
        let dl_sm = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .with_document(&smask_form, &sm_res)
            .interpret(b"q 10 10 10 10 re W n /GS0 gs 0 0 200 200 re f Q");
        let mask_dl = dl_sm
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::PushBlendGroup {
                    mask: Some(mask), ..
                } => Some(&mask.commands),
                _ => None,
            })
            .expect("soft mask emitted");
        // Inside mask_dl, Form /BBox [0 0 200 200] is the detached mask's first
        // clip (kept) and [0 0 100 100] narrows it (NOT falsely elided against
        // the outer page's [10 10 20 20] clip).
        assert_eq!(clip_counts_and_balance(mask_dl), (2, 2, 0));
    }

    #[test]
    fn out_of_range_earlier_clip_does_not_elide_later_rectangle() {
        let page = Rect::new(0.0, 0.0, 100.0, 100.0);

        // First clip spans x = 10..2_000_000, y = 10..20 — finite in PDF space,
        // but rejected by CpuRenderer::finish_within_limits at scale 1. Because
        // the earlier clip is skipped by the backend, the later [0, 0, 100, 30]
        // rectangle must NOT be elided.
        let dl = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 10 10 1999990 10 re W n 0 0 100 30 re W n 0 0 100 100 re f Q");
        assert_eq!(clip_counts_and_balance(&dl), (2, 2, 0));

        // Same guard for an out-of-range Form XObject /BBox.
        let huge_bbox_form = single_object_file(
            "<< /Type /XObject /Subtype /Form /BBox [10 10 2000000 20] /Length 37 >>\n\
             stream\n0 0 100 30 re W n 0 0 100 100 re f\nendstream",
        );
        let mut resources = ResourceDict::default();
        resources.xobjects.insert("FmHuge".into(), ObjectId(1, 0));
        let dl_form = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .with_document(&huge_bbox_form, &resources)
            .interpret(b"/FmHuge Do");
        assert_eq!(clip_counts_and_balance(&dl_form), (2, 2, 0));

        // Contrast: a well-bounded clip [10, 10, 90, 20] IS guaranteed to be
        // enforced, so the covering [0, 0, 100, 30] clip is elided.
        let dl_bounded = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 10 10 80 10 re W n 0 0 100 30 re W n 0 0 100 100 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_bounded), (1, 1, 0));
    }

    #[test]
    fn clip_exceeding_tiny_skia_supersampled_scanline_range_is_kept() {
        // On an 8,388,608 × 1 raster at scale 1, CpuRenderer's coordinate limit
        // is 536,870,912 (1 << 29), but tiny-skia's AA scan converter shifts
        // unclamped `IntRect` top/bottom left by 2 bits in `i32` before
        // clamping. A clip with device bounds `[0, 0, 2, 536_870_912]` wraps
        // negative in tiny-skia and yields an empty mask, so it must be kept.
        let wide_page = Rect::new(0.0, 0.0, 8_388_608.0, 1.0);
        let dl_overflow = ContentInterpreter::new(wide_page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 0 0 2 1 re W n 0 -536870912 2 536870913 re W n 0 0 2 1 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_overflow), (2, 2, 0));

        // Negative top exceeding the safe supersampled range is also kept.
        let dl_neg_overflow = ContentInterpreter::new(wide_page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 0 0 2 1 re W n 0 0 2 536870913 re W n 0 0 2 1 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_neg_overflow), (2, 2, 0));

        // Contrast: a covering rectangle within the safe ±2^28 AA range elides.
        let dl_safe = ContentInterpreter::new(wide_page)
            .with_clip_elision_scale(1.0)
            .interpret(b"q 0 0 2 1 re W n 0 -268435000 2 268435001 re W n 0 0 2 1 re f Q");
        assert_eq!(clip_counts_and_balance(&dl_safe), (1, 1, 0));
    }

    #[test]
    fn type3_bitmap_glyph_with_tiling_pattern_emits_tiling_paint() {
        use std::sync::Arc;
        use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

        // Colored 8×8 tiling pattern painting red left half and blue right half.
        let pat_stream = b"1 0 0 rg 0 0 4 8 re f 0 0 1 rg 4 0 4 8 re f";
        let pat_obj = format!(
            "<< /Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 8 8] /XStep 8 /YStep 8 /Length {} >>\nstream\n{}\nendstream",
            pat_stream.len(),
            std::str::from_utf8(pat_stream).unwrap(),
        );
        let file = single_object_file(&pat_obj);
        let mut resources = ResourceDict::default();
        resources.patterns.insert("P1".into(), ObjectId(1, 0));

        // Type 3 bitmap font with 8×8 `/ImageMask true` stencil (top 4 rows ink).
        let glyph_stream =
            b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\xff\xff\xff\xff EI";
        let mut fonts = FontCache::new();
        let mut encoding = vec![String::new(); 66];
        encoding[65] = "A".into();
        fonts.insert(
            "F1".into(),
            LoadedFont::new_with_data(
                PdfFontType::Type3 {
                    font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                    char_procs: std::collections::HashMap::from([(
                        "A".into(),
                        Arc::<[u8]>::from(&glyph_stream[..]),
                    )]),
                    encoding,
                    widths: vec![1.0],
                    first_char: b'A' as u16,
                },
                "Type3BitmapPat".into(),
                Vec::new(),
                CidWidths::new(1.0),
            ),
        );

        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 40.0, 40.0))
            .with_document(&file, &resources)
            .with_fonts(&mut fonts)
            .interpret(b"/Pattern cs /P1 scn BT /F1 24 Tf 8 8 Td (A) Tj ET");

        let has_tiling_paint = dl.commands.iter().any(|cmd| {
            matches!(
                cmd,
                RenderCommand::FillPath {
                    paint: Paint::Tiling(_),
                    ..
                } | RenderCommand::DrawGlyphRun(GlyphRun {
                    paint: Paint::Tiling(_),
                    ..
                })
            )
        });
        assert!(
            has_tiling_paint,
            "Type 3 bitmap glyph shown under /Pattern cs /P1 scn must emit Paint::Tiling, got {:?}",
            dl.commands
        );
    }

    #[test]
    fn type3_inline_image_cache_distinguishes_rendering_intent() {
        let mut char_procs = HashMap::new();
        let mut glyph_bytes =
            b"1000 0 d0 1000 0 0 1000 0 0 cm BI /W 1 /H 1 /BPC 8 /CS /RGB ID ".to_vec();
        glyph_bytes.extend_from_slice(&[255, 0, 0]);
        glyph_bytes.extend_from_slice(b" EI");
        char_procs.insert("A".to_string(), std::sync::Arc::from(glyph_bytes));
        let mut encoding = vec![String::new(); 66];
        encoding[65] = "A".to_string();
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding,
                widths: vec![1000.0],
                first_char: 65,
            },
            "Type3Intent".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let mut fonts = FontCache::new();
        fonts.insert("F1".into(), font);
        let mut images = zpdf_image::ImageCache::new();
        let page = Rect::new(0.0, 0.0, 200.0, 80.0);

        // First two draws share /Perceptual (must deduplicate to 1 entry);
        // third draw switches to /AbsoluteColorimetric (must miss and create a 2nd entry).
        let dl = ContentInterpreter::new(page)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret(
                b"/Perceptual ri BT /F1 40 Tf 10 20 Td (AA) Tj ET \
                  /AbsoluteColorimetric ri BT /F1 40 Tf 110 20 Td (A) Tj ET",
            );
        let draw_ids: Vec<zpdf_image::ImageId> = dl
            .commands
            .iter()
            .filter_map(|c| match c {
                RenderCommand::DrawImage(d) => Some(d.image_id),
                _ => None,
            })
            .collect();
        assert_eq!(draw_ids.len(), 3);
        assert_eq!(
            draw_ids[0], draw_ids[1],
            "same intent must reuse cached image"
        );
        assert_ne!(
            draw_ids[0], draw_ids[2],
            "distinct rendering intent must not reuse cached image"
        );
        assert_eq!(images.len(), 2);
    }

    #[test]
    fn inner_indirect_properties_shadow_outer_inline_properties() {
        // Object 1: Form XObject with indirect /Properties << /P1 2 0 R >> (MCID 7).
        // Object 2: << /MCID 7 >>.
        // Page has inline /Properties << /P1 << /MCID 99 >> >>.
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let off1 = pdf.len();
        let form_body = b"/Span /P1 BDC BT /F1 12 Tf 10 10 Td (A) Tj ET EMC";
        pdf.extend_from_slice(
            format!(
                "1 0 obj\n<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                 /Resources << /Properties << /P1 2 0 R >> >> /Length {} >>\nstream\n",
                form_body.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(form_body);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let off2 = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /MCID 7 >>\nendobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n0 3\n0000000000 65535 f \n{off1:010} 00000 n \n{off2:010} 00000 n \n\
                 trailer\n<< /Size 3 >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let file = zpdf_parser::PdfFile::parse(pdf).expect("open pdf");

        let mut resources = ResourceDict::default();
        resources.xobjects.insert("Fm1".into(), ObjectId(1, 0));
        let mut outer_prop = PdfDict::new();
        outer_prop.insert(zpdf_core::PdfName::new("MCID"), PdfObject::Integer(99));
        resources.properties_inline.insert("P1".into(), outer_prop);

        let mut fonts = FontCache::new();
        fonts.insert(
            "F1".into(),
            zpdf_font::LoadedFont::new_placeholder("Helvetica".into()),
        );
        let mut spans = Vec::new();
        let _dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &resources)
            .with_fonts(&mut fonts)
            .with_text_sink(&mut spans)
            .interpret(b"/Fm1 Do");

        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].mcid,
            Some(7),
            "inner indirect /Properties /P1 (MCID 7) must shadow page inline /P1 (MCID 99)"
        );
    }

    #[test]
    fn type3_early_budget_stop_omits_expanded_glyph_run() {
        let mut char_procs = HashMap::new();
        char_procs.insert(
            "A".to_string(),
            std::sync::Arc::<[u8]>::from(b"1000 0 d0 0 0 800 800 re f".as_slice()),
        );
        let mut encoding = vec![String::new(); 66];
        encoding[65] = "A".to_string();
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding,
                widths: vec![1000.0],
                first_char: 65,
            },
            "Type3Budget".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let mut fonts = FontCache::new();
        fonts.insert("F1".into(), font);

        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 80.0))
            .with_fonts(&mut fonts)
            .with_command_limit(1)
            .interpret(b"BT /F1 40 Tf 10 20 Td (AA) Tj ET");

        assert_eq!(
            dl.commands.len(),
            1,
            "first glyph's command must be retained when budget stops replay"
        );
        assert!(
            dl.expanded_glyph_runs().is_empty(),
            "partial Type 3 replay stopped by budget must not publish an ExpandedGlyphRun"
        );
    }

    #[test]
    fn type3_inline_image_without_cache_deduplicates_and_respects_budget() {
        let mut glyph_bytes = b"1000 0 0 0 1000 1000 d1\n\
            800 0 0 800 0 0 cm\n\
            BI /IM true /W 2 /H 2 /BPC 1 ID "
            .to_vec();
        glyph_bytes.extend_from_slice(&[0x00, 0x00]);
        glyph_bytes.extend_from_slice(b"\nEI");

        let mut char_procs = HashMap::new();
        char_procs.insert(
            "A".to_string(),
            std::sync::Arc::<[u8]>::from(glyph_bytes.as_slice()),
        );
        let mut encoding = vec![String::new(); 66];
        encoding[65] = "A".to_string();
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding,
                widths: vec![1000.0],
                first_char: 65,
            },
            "Type3InlineDedup".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let mut fonts = FontCache::new();
        fonts.insert("F1".into(), font);

        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 80.0))
            .with_fonts(&mut fonts)
            .interpret(b"1 0 0 rg BT /F1 40 Tf 10 20 Td (AA) Tj ET");
        let inline_draws: Vec<_> = dl
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                RenderCommand::DrawInlineImage(draw) => Some(draw),
                _ => None,
            })
            .collect();
        assert_eq!(
            inline_draws.len(),
            2,
            "both glyph instances emit inline draws"
        );
        assert!(
            std::sync::Arc::ptr_eq(&inline_draws[0].image, &inline_draws[1].image),
            "repeated Type 3 inline images without ImageCache must share one decoded Arc"
        );

        // Tight budget smaller than a 2x2 RGBA buffer (16 bytes) drops the inline
        // image and records `resource_limit_reached`.
        let tight = zpdf_core::ParseLimits {
            max_image_cache_bytes: 8,
            ..zpdf_core::ParseLimits::default()
        };
        let (dl_tight, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 80.0))
            .with_fonts(&mut fonts)
            .with_limits(&tight)
            .interpret_with_stats(b"1 0 0 rg BT /F1 40 Tf 10 20 Td (A) Tj ET");
        assert!(
            dl_tight.commands.is_empty(),
            "inline image exceeding max_image_cache_bytes must be dropped"
        );
        assert!(
            stats.diagnostics.resource_limit_reached,
            "dropping an over-budget inline image must report resource_limit_reached"
        );
    }

    #[test]
    fn type3_inline_image_stopped_by_operator_budget_preserves_cutoff() {
        // CharProc paints an inline stencil, then a vector rectangle. Under an
        // operator budget that expires right after the inline image (with no
        // ImageCache), the interpreter must preserve the stopping point: emit
        // only the pre-cutoff DrawInlineImage, never emit a DrawGlyphRun that
        // would replay the post-cutoff FillPath, and omit ExpandedGlyphRun.
        let mut glyph_bytes = b"1000 0 d0\n\
            800 0 0 800 0 0 cm\n\
            BI /IM true /W 1 /H 1 /BPC 1 ID "
            .to_vec();
        glyph_bytes.push(0x00);
        glyph_bytes.extend_from_slice(b"\nEI\n0 1 0 rg 0 0 500 500 re f");

        let mut char_procs = HashMap::new();
        char_procs.insert(
            "A".to_string(),
            std::sync::Arc::<[u8]>::from(glyph_bytes.as_slice()),
        );
        let mut encoding = vec![String::new(); 66];
        encoding[65] = "A".to_string();
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding,
                widths: vec![1000.0],
                first_char: 65,
            },
            "Type3OpCutoff".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let mut fonts = FontCache::new();
        fonts.insert("F1".into(), font);

        // Page ops: BT (1), Tf (2), Td (3), Tj (4) -> inside CharProc: d0 (5),
        // cm (6), BI/EI (7). Cap at 7 operators so `0 1 0 rg 0 0 500 500 re f`
        // is cut off immediately after the inline image.
        let limits = zpdf_core::ParseLimits {
            max_page_operators: 7,
            ..zpdf_core::ParseLimits::default()
        };
        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_fonts(&mut fonts)
            .with_limits(&limits)
            .interpret(b"BT /F1 40 Tf 10 20 Td (A) Tj ET");

        assert_eq!(
            dl.commands.len(),
            1,
            "only the pre-cutoff inline image must be emitted"
        );
        assert!(matches!(dl.commands[0], RenderCommand::DrawInlineImage(_)));
        assert!(
            !dl.commands.iter().any(|c| matches!(
                c,
                RenderCommand::DrawGlyphRun(_) | RenderCommand::FillPath { .. }
            )),
            "cutoff after inline image must not emit DrawGlyphRun or post-cutoff FillPath"
        );
        assert!(
            dl.expanded_glyph_runs().is_empty(),
            "interrupted glyph must not publish an ExpandedGlyphRun"
        );
    }

    #[test]
    fn type3_glyph_resources_share_indirect_scope_and_bound_distinct_scopes() {
        let stream_obj = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objs: &[Vec<u8>]| -> Vec<u8> {
            let mut out = b"%PDF-1.7\n".to_vec();
            let mut offs = Vec::with_capacity(objs.len());
            for (i, body) in objs.iter().enumerate() {
                offs.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                out.extend_from_slice(body);
                out.extend_from_slice(b"\nendobj\n");
            }
            let xref = out.len();
            let n = objs.len() + 1;
            out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
            for off in offs {
                out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(
                format!("trailer\n<< /Size {n} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
            );
            out
        };

        // 1. Two distinct CharProcs (obj 1 and obj 2) share one indirect
        //    `/Resources 3 0 R` carrying a large inline ExtGState payload.
        // 2. Two other CharProcs (obj 4 and obj 5) each carry distinct inline
        //    `/Resources` dictionaries with large inline payloads.
        let shared_payload = "s".repeat(2048);
        let inline_payload = "i".repeat(2048);
        let shared_res =
            format!("<< /ExtGState << /GS1 << /ca 0.5 /Pad ({shared_payload}) >> >> >>");
        let inline_res =
            format!("/Resources << /ExtGState << /GS1 << /ca 0.5 /Pad ({inline_payload}) >> >> >>");
        let pdf = assemble(&[
            stream_obj("/Resources 3 0 R", b"1000 0 d0 /GS1 gs 0 0 500 500 re f"), // 1
            stream_obj("/Resources 3 0 R", b"1000 0 d0 /GS1 gs 0 0 500 500 re f"), // 2
            shared_res.into_bytes(),                                               // 3
            stream_obj(&inline_res, b"1000 0 d0 /GS1 gs 0 0 500 500 re f"),        // 4
            stream_obj(&inline_res, b"1000 0 d0 /GS1 gs 0 0 500 500 re f"),        // 5
        ]);
        let file = zpdf_parser::PdfFile::parse(pdf).expect("parse pdf");
        let page_res = ResourceDict::default();
        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &page_res);
        interp.max_type3_resource_cache_bytes = 4096;

        let s1 = interp
            .cached_type3_glyph_resources(ObjectId(1, 0))
            .expect("cp 1 scope");
        let bytes_after_first = interp.type3_resource_cache_bytes;
        let s2 = interp
            .cached_type3_glyph_resources(ObjectId(2, 0))
            .expect("cp 2 scope");
        assert!(
            std::sync::Arc::ptr_eq(&s1, &s2),
            "distinct CharProcs pointing to the same indirect /Resources must share one Arc"
        );
        assert!(
            std::sync::Arc::ptr_eq(
                s1.parsed_resources.as_ref().expect("parsed"),
                s2.parsed_resources.as_ref().expect("parsed"),
            ),
            "parsed ResourceDict must be shared, not cloned per CharProc"
        );
        assert!(
            interp.type3_resource_cache_bytes <= bytes_after_first + 48,
            "second CharProc sharing an indirect /Resources must not multiply retained bytes"
        );

        // Distinct inline scopes past `max_type3_resource_cache_bytes` still
        // parse and return usable scopes, but are not retained past the cap.
        let s4 = interp
            .cached_type3_glyph_resources(ObjectId(4, 0))
            .expect("cp 4 scope still returned");
        let s5 = interp
            .cached_type3_glyph_resources(ObjectId(5, 0))
            .expect("cp 5 scope still returned");
        assert!(s4.parsed_resources.is_some());
        assert!(s5.parsed_resources.is_some());
        assert!(
            interp.type3_resource_cache_bytes <= interp.max_type3_resource_cache_bytes,
            "retained Type 3 resource cache bytes ({}) must stay within cap ({})",
            interp.type3_resource_cache_bytes,
            interp.max_type3_resource_cache_bytes
        );
        assert!(
            !interp.type3_glyph_resources.contains_key(&ObjectId(4, 0)),
            "over-budget distinct inline glyph scope must not be retained"
        );
        assert!(
            !interp.type3_glyph_resources.contains_key(&ObjectId(5, 0)),
            "over-budget distinct inline glyph scope must not be retained"
        );
    }

    #[test]
    fn direct_type3_resource_scope_does_not_leak_cache_budget_when_entry_rejected() {
        let stream_obj = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objs: &[Vec<u8>]| -> Vec<u8> {
            let mut out = b"%PDF-1.7\n".to_vec();
            let mut offs = Vec::with_capacity(objs.len());
            for (i, body) in objs.iter().enumerate() {
                offs.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                out.extend_from_slice(body);
                out.extend_from_slice(b"\nendobj\n");
            }
            let xref = out.len();
            let n = objs.len() + 1;
            out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
            for off in offs {
                out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(
                format!("trailer\n<< /Size {n} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
            );
            out
        };

        let payload = "p".repeat(2048);
        let inline_res =
            format!("/Resources << /ExtGState << /GS1 << /ca 0.5 /Pad ({payload}) >> >> >>");
        let pdf = assemble(&[stream_obj(&inline_res, b"1000 0 d0 0 0 500 500 re f")]);
        let file = zpdf_parser::PdfFile::parse(pdf).expect("parse pdf");
        let page_res = ResourceDict::default();

        // Measure the exact direct scope cost, then set the cap to `scope_cost + 16`:
        // enough for the scope alone, but 32 bytes short of `scope_cost + 48`.
        let mut probe = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &page_res);
        let scope = probe
            .cached_type3_glyph_resources(ObjectId(1, 0))
            .expect("probe scope");
        let scope_cost = estimate_type3_resource_scope_bytes(&scope);

        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &page_res);
        interp.max_type3_resource_cache_bytes = scope_cost + 16;

        let returned = interp
            .cached_type3_glyph_resources(ObjectId(1, 0))
            .expect("scope still returned for immediate replay");
        assert!(returned.parsed_resources.is_some());
        assert!(
            !interp.type3_glyph_resources.contains_key(&ObjectId(1, 0)),
            "entry must not be inserted when scope_cost + 48 exceeds cap"
        );
        assert_eq!(
            interp.type3_resource_cache_bytes, 0,
            "rejected direct scope must not leak bytes into type3_resource_cache_bytes"
        );
    }

    #[test]
    fn rejected_ownerless_inline_type3_source_does_not_replay_against_parent_resources() {
        let stream_obj = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objs: &[Vec<u8>]| -> Vec<u8> {
            let mut out = b"%PDF-1.7\n".to_vec();
            let mut offs = Vec::with_capacity(objs.len());
            for (i, body) in objs.iter().enumerate() {
                offs.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                out.extend_from_slice(body);
                out.extend_from_slice(b"\nendobj\n");
            }
            let xref = out.len();
            let n = objs.len() + 1;
            out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
            for off in offs {
                out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(
                format!("trailer\n<< /Size {n} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
            );
            out
        };

        // Obj 1: parent's /Stamp Form (paints RED).
        // Obj 2: Type 3 font's /Stamp Form (paints GREEN).
        // Obj 3: CharProc /A invoking `/Stamp Do`.
        let pdf = assemble(&[
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100]",
                b"1 0 0 rg 0 0 100 100 re f",
            ),
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100]",
                b"0 1 0 rg 0 0 100 100 re f",
            ),
            stream_obj("", b"1000 0 d0 /Stamp Do"),
        ]);
        let file = zpdf_parser::PdfFile::parse(pdf).expect("parse pdf");

        let mut page_res = ResourceDict::default();
        page_res.xobjects.insert("Stamp".into(), ObjectId(1, 0));

        // Build an owner-less inline Type 3 font whose small CharProc fits in the
        // font budget, while its large `/Pad` entry makes the inline source dict
        // exceed `max_font_cache_bytes`.
        let mut char_procs = zpdf_core::PdfDict::new();
        char_procs.insert(zpdf_core::PdfName::new("A"), PdfObject::Ref(ObjectId(3, 0)));
        let mut enc_dict = zpdf_core::PdfDict::new();
        enc_dict.insert(
            zpdf_core::PdfName::new("Differences"),
            PdfObject::Array(vec![
                PdfObject::Integer(65),
                PdfObject::Name(zpdf_core::PdfName::new("A")),
            ]),
        );
        let mut font_xobjs = zpdf_core::PdfDict::new();
        font_xobjs.insert(
            zpdf_core::PdfName::new("Stamp"),
            PdfObject::Ref(ObjectId(2, 0)),
        );
        let mut font_res = zpdf_core::PdfDict::new();
        font_res.insert(
            zpdf_core::PdfName::new("XObject"),
            PdfObject::Dict(font_xobjs),
        );
        let mut font_dict = zpdf_core::PdfDict::new();
        font_dict.insert(
            zpdf_core::PdfName::new("Type"),
            PdfObject::Name(zpdf_core::PdfName::new("Font")),
        );
        font_dict.insert(
            zpdf_core::PdfName::new("Subtype"),
            PdfObject::Name(zpdf_core::PdfName::new("Type3")),
        );
        font_dict.insert(
            zpdf_core::PdfName::new("FontMatrix"),
            PdfObject::Array(vec![
                PdfObject::Real(0.001),
                PdfObject::Real(0.0),
                PdfObject::Real(0.0),
                PdfObject::Real(0.001),
                PdfObject::Real(0.0),
                PdfObject::Real(0.0),
            ]),
        );
        font_dict.insert(
            zpdf_core::PdfName::new("CharProcs"),
            PdfObject::Dict(char_procs),
        );
        font_dict.insert(
            zpdf_core::PdfName::new("Encoding"),
            PdfObject::Dict(enc_dict),
        );
        font_dict.insert(zpdf_core::PdfName::new("FirstChar"), PdfObject::Integer(65));
        font_dict.insert(zpdf_core::PdfName::new("LastChar"), PdfObject::Integer(65));
        font_dict.insert(
            zpdf_core::PdfName::new("Widths"),
            PdfObject::Array(vec![PdfObject::Integer(1000)]),
        );
        font_dict.insert(
            zpdf_core::PdfName::new("Resources"),
            PdfObject::Dict(font_res),
        );
        font_dict.insert(
            zpdf_core::PdfName::new("Pad"),
            PdfObject::String(zpdf_core::PdfString::new(vec![b'x'; 16_384])),
        );

        let mut form_fonts = zpdf_core::PdfDict::new();
        form_fonts.insert(zpdf_core::PdfName::new("F3"), PdfObject::Dict(font_dict));
        let mut synth_res = zpdf_core::PdfDict::new();
        synth_res.insert(zpdf_core::PdfName::new("Font"), PdfObject::Dict(form_fonts));

        // Synthetic owner-less Form XObject (xobj_id = None) holding the inline Type 3 font.
        let mut synth_dict = zpdf_core::PdfDict::new();
        synth_dict.insert(
            zpdf_core::PdfName::new("Resources"),
            PdfObject::Dict(synth_res),
        );
        synth_dict.insert(
            zpdf_core::PdfName::new("BBox"),
            PdfObject::Array(vec![
                PdfObject::Integer(0),
                PdfObject::Integer(0),
                PdfObject::Integer(100),
                PdfObject::Integer(100),
            ]),
        );
        let synth_form = zpdf_core::PdfStream {
            dict: synth_dict,
            data: zpdf_core::PdfStreamData::from(b"BT /F3 50 Tf 0 0 Td (A) Tj ET".as_slice()),
        };

        let limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: 12_000,
            ..zpdf_core::ParseLimits::default()
        };
        let mut fc = FontCache::new();
        let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_document(&file, &page_res)
            .with_fonts(&mut fc)
            .with_limits(&limits);
        interp.do_form_xobject(None, &synth_form, &file);

        assert!(
            interp.stats.diagnostics.resource_limit_reached,
            "rejected owner-less Type 3 source must report resource_limit_reached"
        );
        assert!(
            !interp.display_list.commands.iter().any(is_paint_command),
            "rejected owner-less Type 3 font must not replay against parent's red /Stamp, got {:?}",
            interp.display_list.commands
        );
        assert!(
            fc.get(0).is_some_and(|f| f.has_type3_programs()),
            "test setup must admit the small Type 3 program while rejecting its oversized source dict"
        );
    }

    #[test]
    fn ownerless_inline_type3_recovers_when_reloadable_source_encountered_in_either_order() {
        let stream_obj = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objs: &[Vec<u8>]| -> Vec<u8> {
            let mut out = b"%PDF-1.7\n".to_vec();
            let mut offs = Vec::with_capacity(objs.len());
            for (i, body) in objs.iter().enumerate() {
                offs.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                out.extend_from_slice(body);
                out.extend_from_slice(b"\nendobj\n");
            }
            let xref = out.len();
            let n = objs.len() + 1;
            out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
            for off in offs {
                out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(
                format!("trailer\n<< /Size {n} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
            );
            out
        };

        // Oversized inline Type 3 font dictionary: small CharProc fits in 12 KiB,
        // while 16 KiB `/Pad` exceeds `max_font_cache_bytes`.
        let pad = "x".repeat(16_384);
        let inline_font_pdf = format!(
            "<< /Type /Font /Subtype /Type3 /FontMatrix [0.001 0 0 0.001 0 0] \
             /CharProcs << /A 3 0 R >> /Encoding << /Differences [65 /A] >> \
             /FirstChar 65 /LastChar 65 /Widths [1000] \
             /Resources << /XObject << /Stamp 2 0 R >> >> \
             /Pad ({pad}) >>"
        );
        let form_with_inline_res = stream_obj(
            &format!(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                 /Resources << /Font << /F3 {inline_font_pdf} >> >>"
            ),
            b"BT /F3 50 Tf 0 0 Td (A) Tj ET",
        );
        let indirect_res_dict = format!("<< /Font << /F3 {inline_font_pdf} >> >>").into_bytes();
        let form_with_indirect_res = stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources 5 0 R",
            b"BT /F3 50 Tf 0 0 Td (A) Tj ET",
        );

        // Obj 1: parent RED /Stamp; Obj 2: Type 3 GREEN /Stamp; Obj 3: CharProc /A;
        // Obj 4: indirect Form with inline /Resources; Obj 5: indirect /Resources;
        // Obj 6: Form pointing to indirect /Resources 5 0 R.
        let pdf = assemble(&[
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100]",
                b"1 0 0 rg 0 0 100 100 re f",
            ),
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100]",
                b"0 1 0 rg 0 0 100 100 re f",
            ),
            stream_obj("", b"1000 0 d0 /Stamp Do"),
            form_with_inline_res,
            indirect_res_dict,
            form_with_indirect_res,
        ]);
        let file = zpdf_parser::PdfFile::parse(pdf).expect("parse pdf");
        let container_stream = file
            .resolve(ObjectId(4, 0))
            .expect("obj 4")
            .as_stream()
            .expect("form stream 4")
            .clone();
        let indirect_res_stream = file
            .resolve(ObjectId(6, 0))
            .expect("obj 6")
            .as_stream()
            .expect("form stream 6")
            .clone();
        // Owner-less synthetic Form carrying the identical inline font dict.
        let ownerless_stream = container_stream.clone();

        let mut page_res = ResourceDict::default();
        page_res.xobjects.insert("Stamp".into(), ObjectId(1, 0));

        let limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: 12_000,
            ..zpdf_core::ParseLimits::default()
        };

        let count_green_and_red = |cmds: &[RenderCommand]| -> (usize, usize) {
            let mut green = 0usize;
            let mut red = 0usize;
            for cmd in cmds {
                if let RenderCommand::FillPath {
                    paint: Paint::Solid(c),
                    ..
                } = cmd
                {
                    if c.g > 0.9 && c.r < 0.1 {
                        green += 1;
                    }
                    if c.r > 0.9 && c.g < 0.1 {
                        red += 1;
                    }
                }
            }
            (green, red)
        };

        // Order 1: owner-less first (Unavailable) -> indirect Form container second (recovers).
        {
            let mut fc = FontCache::new();
            let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
                .with_document(&file, &page_res)
                .with_fonts(&mut fc)
                .with_limits(&limits);
            interp.do_form_xobject(None, &ownerless_stream, &file);
            assert_eq!(
                count_green_and_red(&interp.display_list.commands),
                (0, 0),
                "owner-less oversized font must not paint before a reloadable source is seen"
            );
            interp.do_form_xobject(Some(ObjectId(4, 0)), &container_stream, &file);
            // Subsequent draw of the owner-less form now also reuses the recovered source.
            interp.do_form_xobject(None, &ownerless_stream, &file);
            assert_eq!(
                count_green_and_red(&interp.display_list.commands),
                (2, 0),
                "indirect Form container must upgrade Unavailable so both subsequent draws paint green /Stamp"
            );
            assert!(
                fc.bytes_used() <= limits.max_font_cache_bytes,
                "recovered container source must stay within budget"
            );
        }

        // Order 2: indirect Form container first -> owner-less second (both paint green).
        {
            let mut fc = FontCache::new();
            let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
                .with_document(&file, &page_res)
                .with_fonts(&mut fc)
                .with_limits(&limits);
            interp.do_form_xobject(Some(ObjectId(4, 0)), &container_stream, &file);
            interp.do_form_xobject(None, &ownerless_stream, &file);
            assert_eq!(
                count_green_and_red(&interp.display_list.commands),
                (2, 0),
                "container-first encounter order must paint green /Stamp in both forms"
            );
            assert!(
                fc.bytes_used() <= limits.max_font_cache_bytes,
                "container-first source must stay within budget"
            );
        }

        // Order 3: owner-less first (Unavailable) -> indirect /Resources dict second (recovers).
        {
            let mut fc = FontCache::new();
            let mut interp = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
                .with_document(&file, &page_res)
                .with_fonts(&mut fc)
                .with_limits(&limits);
            interp.do_form_xobject(None, &ownerless_stream, &file);
            assert_eq!(count_green_and_red(&interp.display_list.commands), (0, 0));
            interp.do_form_xobject(None, &indirect_res_stream, &file);
            assert_eq!(
                count_green_and_red(&interp.display_list.commands),
                (1, 0),
                "indirect /Resources dict must upgrade Unavailable and paint green /Stamp"
            );
            assert!(
                fc.bytes_used() <= limits.max_font_cache_bytes,
                "recovered /Resources source must stay within budget"
            );
        }
    }

    #[test]
    fn type3_d1_anisotropic_stroke_with_tiling_pattern_preserves_axis_widths() {
        // Real PDF with a Type 3 `d1` CharProc stroking a vertical segment
        // (x=4, y=2..38) and horizontal segment (x=12..22, y=20) with `2 w`
        // under a 4×1 nonuniform text transform (`400 Tz` or `4 0 0 1 8 8 Tm`),
        // painted with a tiling pattern. Vertical stroke must span 8 pt wide
        // (x=20..28) and horizontal stroke 2 pt tall (y=27..29), NOT 5×5.
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

        let run_pdf = |page_stream: &[u8], charproc: &[u8]| -> DisplayList {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 110 56] /Contents 4 0 R \
                  /Resources << /Font << /F1 5 0 R >> /Pattern << /P1 9 0 R >> >> >>"
                    .to_vec(),
                stream("", page_stream),
                b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 30 40] \
                  /FontMatrix [1 0 0 1 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
                  /FirstChar 65 /LastChar 65 /Widths [30] /Resources << >> >>"
                    .to_vec(),
                b"<< /A 8 0 R >>".to_vec(),
                b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(),
                stream("", charproc),
                stream(
                    "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                     /BBox [0 0 8 8] /XStep 8 /YStep 8 /Resources << >>",
                    b"1 0 0 rg 0 0 4 8 re f 0 0 1 rg 4 0 4 8 re f",
                ),
            ]);
            let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let mut images = ImageCache::new();
            let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
            let (dl, _) = ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .with_images(&mut images)
                .interpret_with_stats(&content);
            dl
        };

        // 1) Patterned `d1` CharProc under `400 Tz` AND under `4 0 0 1 8 8 Tm`.
        let charproc = b"30 0 0 0 30 40 d1 2 w 4 2 m 4 38 l S 12 20 m 22 20 l S";
        for page_stream in [
            &b"/Pattern cs /P1 scn BT /F1 1 Tf 400 Tz 8 8 Td (A) Tj ET"[..],
            &b"/Pattern cs /P1 scn BT /F1 1 Tf 4 0 0 1 8 8 Tm (A) Tj ET"[..],
        ] {
            let dl = run_pdf(page_stream, charproc);
            let tiling_fills: Vec<Rect> = dl
                .commands
                .iter()
                .filter_map(|cmd| match cmd {
                    RenderCommand::FillPath {
                        path,
                        paint: Paint::Tiling(_),
                        ..
                    } => ContentInterpreter::path_bounds(path),
                    _ => None,
                })
                .collect();
            assert_eq!(
                tiling_fills.len(),
                2,
                "anisotropic 4x1 Type 3 strokes must emit 2 tiling FillPath outlines"
            );
            // Vertical stroke at x=24, y=10..46 -> width=8, height=36.
            let v = tiling_fills[0];
            assert!(
                (v.width() - 8.0).abs() < 0.05 && (v.height() - 36.0).abs() < 0.05,
                "vertical stroke must be 8x36 pt, got {v:?}"
            );
            // Horizontal stroke at x=56..96, y=28 -> width=40, height=2.
            let h = tiling_fills[1];
            assert!(
                (h.width() - 40.0).abs() < 0.05 && (h.height() - 2.0).abs() < 0.05,
                "horizontal stroke must be 40x2 pt, got {h:?}"
            );
        }

        // 2) Isotropic 1×1 transform keeps native `StrokePath` (unchanged).
        let iso_dl = run_pdf(
            b"/Pattern cs /P1 scn BT /F1 1 Tf 8 8 Td (A) Tj ET",
            charproc,
        );
        assert_eq!(
            iso_dl
                .commands
                .iter()
                .filter(|cmd| matches!(
                    cmd,
                    RenderCommand::StrokePath {
                        paint: Paint::Tiling(_),
                        ..
                    }
                ))
                .count(),
            2,
            "isotropic Type 3 strokes must stay RenderCommand::StrokePath"
        );

        // 3) Hairline (`0 w`) under 4×1 transform stays `StrokePath { width: 0.0 }`.
        let hairline_dl = run_pdf(
            b"/Pattern cs /P1 scn BT /F1 1 Tf 400 Tz 8 8 Td (A) Tj ET",
            b"30 0 0 0 30 40 d1 0 w 4 2 m 4 38 l S",
        );
        assert!(
            hairline_dl.commands.iter().any(|cmd| matches!(
                cmd,
                RenderCommand::StrokePath {
                    style,
                    paint: Paint::Tiling(_),
                    ..
                } if style.width == 0.0
            )),
            "0-width hairline under anisotropic CTM must remain StrokePath with width 0"
        );

        // 4) Dashed anisotropic stroke transforms dash lengths anisotropically
        //    ([4 4] 0 d on a 12-unit horizontal segment under 4×1 -> 2 dashes of 16pt each,
        //    total width 48pt with 16pt gap in the middle).
        let dashed_dl = run_pdf(
            b"/Pattern cs /P1 scn BT /F1 1 Tf 400 Tz 8 8 Td (A) Tj ET",
            b"30 0 0 0 30 40 d1 2 w [4 4] 0 d 0 20 m 12 20 l S",
        );
        let dashed_path = dashed_dl
            .commands
            .iter()
            .find_map(|cmd| match cmd {
                RenderCommand::FillPath {
                    path,
                    paint: Paint::Tiling(_),
                    ..
                } => Some(path),
                _ => None,
            })
            .expect("dashed anisotropic stroke must expand to FillPath");
        let subpath_count = dashed_path
            .iter()
            .filter(|e| matches!(e, PathElement::MoveTo(_)))
            .count();
        assert_eq!(
            subpath_count, 2,
            "12-unit segment with [4 4] dash must produce 2 separate dash contours"
        );
    }

    #[test]
    fn repeated_type3_glyph_with_own_resources_reuses_cached_tiling_cell() {
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

        // Type 3 font with:
        // - Font-level inline /Resources << /Pattern << /PF 11 0 R >> >>
        // - Glyph /A with its own inline /Resources << /Pattern << /PA 11 0 R >> >>
        // - Glyph /B with an indirect /Resources 12 0 R
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream("", b"BT /F1 10 Tf 10 20 Td (AAAAAAAAAA) Tj 0 20 Td (BBBBBBBBBB) Tj 0 20 Td (CCCCCCCCCC) Tj ET"), // 4
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] \
              /FontMatrix [0.1 0 0 0.1 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 67 /Widths [10 10 10] \
              /Resources << /Pattern << /PF 11 0 R >> >> >>"
                .to_vec(), // 5
            b"<< /A 8 0 R /B 9 0 R /C 10 0 R >>".to_vec(), // 6
            b"<< /Type /Encoding /Differences [65 /A /B /C] >>".to_vec(), // 7
            stream(
                "/Resources << /Pattern << /PA 11 0 R >> >>",
                b"10 0 d0 /Pattern cs /PA scn 0 0 10 10 re f",
            ), // 8
            stream(
                "/Resources 12 0 R",
                b"10 0 d0 /Pattern cs /PB scn 0 0 10 10 re f",
            ), // 9
            stream("", b"10 0 d0 /Pattern cs /PF scn 0 0 10 10 re f"), // 10
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 4 4] /XStep 4 /YStep 4 /Resources << >>",
                b"1 0 0 rg 0 0 2 4 re f",
            ), // 11
            b"<< /Pattern << /PB 11 0 R >> >>".to_vec(), // 12
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let mut images = ImageCache::new();
        let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let (dl, _) = ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content);

        let cells: Vec<std::sync::Arc<DisplayList>> = dl
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                RenderCommand::FillPath {
                    paint: Paint::Tiling(t),
                    ..
                } => Some(std::sync::Arc::clone(&t.commands)),
                _ => None,
            })
            .collect();
        assert_eq!(cells.len(), 30, "expected 10 each of A, B, and C");

        // All 10 occurrences of A (glyph inline /Resources) must share one cell Arc.
        for cell in &cells[1..10] {
            assert!(
                std::sync::Arc::ptr_eq(&cells[0], cell),
                "repeated glyph A with inline /Resources must share cached tiling cell DisplayList"
            );
        }
        // All 10 occurrences of B (glyph indirect /Resources) must share one cell Arc.
        for cell in &cells[11..20] {
            assert!(
                std::sync::Arc::ptr_eq(&cells[10], cell),
                "repeated glyph B with indirect /Resources must share cached tiling cell DisplayList"
            );
        }
        // All 10 occurrences of C (font-level /Resources) must share one cell Arc.
        for cell in &cells[21..30] {
            assert!(
                std::sync::Arc::ptr_eq(&cells[20], cell),
                "repeated glyph C with font-level /Resources must share cached tiling cell DisplayList"
            );
        }
    }

    #[test]
    fn anisotropic_long_offpage_dashed_line_preserves_dashes_and_phase() {
        // 4×1 anisotropic CTM on page [0..100]×[0..60], 2-unit pen, [5 5] 0 d
        // (period_user = 10 -> period_page_x = 40: 20pt on, 20pt off).
        // Line spans user x = -1_000_000 .. +1_000_000 (total_len / period = 200_000
        // > MAX_DASH_REPEATS = 100_000), triggering the dash-repeat guard.
        let run = |start_x: i64| -> DisplayList {
            let stream = format!("q 4 0 0 1 0 0 cm 2 w [5 5] 0 d {start_x} 30 m 1000000 30 l S Q");
            let (dl, _) = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 60.0))
                .interpret_with_stats(stream.as_bytes());
            dl
        };

        // Start at -1_000_000 (exact multiple of period_user=10):
        // x_user in [0..5] (page [0..20]) is ON, [5..10] (page [20..40]) is OFF,
        // [10..15] (page [40..60]) is ON, [15..20] (page [60..80]) is OFF,
        // [20..25] (page [80..100]) is ON.
        let dl_a = run(-1_000_000);
        let path_a = dl_a
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath { path, .. } => Some(path),
                _ => None,
            })
            .expect("long clipped dashed anisotropic line must expand to FillPath dashes");

        let collect_dash_x_intervals = |path: &Path| -> Vec<(f64, f64, f64)> {
            let mut intervals = Vec::new();
            let mut x0 = f64::INFINITY;
            let mut x1 = f64::NEG_INFINITY;
            let mut y0 = f64::INFINITY;
            let mut y1 = f64::NEG_INFINITY;
            let flush = |intervals: &mut Vec<(f64, f64, f64)>,
                         x0: &mut f64,
                         x1: &mut f64,
                         y0: &mut f64,
                         y1: &mut f64| {
                if x0.is_finite() && x1.is_finite() {
                    intervals.push((*x0, *x1, *y1 - *y0));
                }
                *x0 = f64::INFINITY;
                *x1 = f64::NEG_INFINITY;
                *y0 = f64::INFINITY;
                *y1 = f64::NEG_INFINITY;
            };
            for elem in path {
                match elem {
                    PathElement::MoveTo(p) => {
                        flush(&mut intervals, &mut x0, &mut x1, &mut y0, &mut y1);
                        x0 = p.x;
                        x1 = p.x;
                        y0 = p.y;
                        y1 = p.y;
                    }
                    PathElement::LineTo(p) => {
                        x0 = x0.min(p.x);
                        x1 = x1.max(p.x);
                        y0 = y0.min(p.y);
                        y1 = y1.max(p.y);
                    }
                    PathElement::CurveTo(c1, c2, end) => {
                        for p in [c1, c2, end] {
                            x0 = x0.min(p.x);
                            x1 = x1.max(p.x);
                            y0 = y0.min(p.y);
                            y1 = y1.max(p.y);
                        }
                    }
                    PathElement::Close => {}
                }
            }
            flush(&mut intervals, &mut x0, &mut x1, &mut y0, &mut y1);
            intervals
        };

        let dashes_a = collect_dash_x_intervals(path_a);
        assert!(
            dashes_a.len() >= 3,
            "expected multiple distinct visible dashes (not a solid line), got {dashes_a:?}"
        );
        // Every horizontal dash must preserve 2pt height (NOT averaged 5pt).
        for &(_, _, h) in &dashes_a {
            assert!(
                (h - 2.0).abs() < 0.05,
                "4x1 horizontal dash must be 2pt tall, got {h}"
            );
        }
        // Page x=10, 50, 90 must be covered; gaps x=30, 70 must be empty.
        let covers = |dashes: &[(f64, f64, f64)], x: f64| {
            dashes.iter().any(|&(a, b, _)| x >= a - 0.1 && x <= b + 0.1)
        };
        assert!(covers(&dashes_a, 10.0) && covers(&dashes_a, 50.0) && covers(&dashes_a, 90.0));
        assert!(
            !covers(&dashes_a, 30.0) && !covers(&dashes_a, 70.0),
            "dash gaps at x=30 and x=70 must stay empty, got {dashes_a:?}"
        );

        // Shift start by half a period (-1_000_005): phase flips so [0..20] and
        // [40..60] become OFF while [20..40] and [60..80] become ON.
        let dl_b = run(-1_000_005);
        let path_b = dl_b
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath { path, .. } => Some(path),
                _ => None,
            })
            .expect("phase-shifted clipped dashed line must expand to FillPath");
        let dashes_b = collect_dash_x_intervals(path_b);
        assert!(
            covers(&dashes_b, 30.0) && covers(&dashes_b, 70.0),
            "half-period shifted start must place dashes at x=30 and x=70, got {dashes_b:?}"
        );
        assert!(
            !covers(&dashes_b, 10.0) && !covers(&dashes_b, 50.0),
            "half-period shifted start must leave gaps at x=10 and x=50, got {dashes_b:?}"
        );
    }

    #[test]
    fn nested_tiling_in_scaled_form_and_translated_page_hatch_stay_aligned() {
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

        // 1) A 2× Form defining both outer tile /POut and inner hatch /PHatch:
        //    /POut inherits /PHatch from the Form's /Resources. Inside /POut's
        //    cell, /PHatch's matrix must be identity (NOT 2×), so rendering
        //    /POut at 2× draws /PHatch at 2× (NOT 4×).
        // 2) Two translated placements of /FormPageRef (at dx=0 and dx=5, half of
        //    the 10pt page hatch period):
        //    - /PInherit looks up /PageHatch from page /Resources -> must NOT
        //      reuse Placement 1's cell with stale translate(0, 0); Placement 2's
        //      inner pattern matrix must carry translate(-5, 0) so page hatch
        //      stays anchored at page x=0.
        //    - /PSelf is self-contained inside the same Form -> MUST reuse the
        //      exact same `Arc<DisplayList>` across both translated placements!
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 80] /Contents 4 0 R \
              /Resources << /XObject << /FormScaled 5 0 R /FormTrans 8 0 R >> \
                            /Pattern << /PageHatch 11 0 R >> >> >>"
                .to_vec(), // 3
            stream(
                "",
                b"q 2 0 0 2 0 0 cm /FormScaled Do Q \
                  q 1 0 0 1 0 0 cm /FormTrans Do Q \
                  q 1 0 0 1 5 0 cm /FormTrans Do Q",
            ), // 4
            // 5: /FormScaled defines both /POut (6) and /PHatch (7)
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 20 20] \
                 /Resources << /Pattern << /POut 6 0 R /PHatch 7 0 R >> >>",
                b"/Pattern cs /POut scn 0 0 20 20 re f",
            ),
            // 6: /POut fills its cell with Form-inherited /PHatch
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>",
                b"/Pattern cs /PHatch scn 0 0 20 20 re f",
            ),
            // 7: /PHatch 10×10 stripe pattern
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
                b"1 0 0 rg 0 0 5 10 re f",
            ),
            // 8: /FormTrans draws both /PInherit (9, inherits /PageHatch) and /PSelf (10, self-contained)
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 40 20] \
                 /Resources << /Pattern << /PInherit 9 0 R /PSelf 10 0 R >> >>",
                b"/Pattern cs /PInherit scn 0 0 20 20 re f /Pattern cs /PSelf scn 20 0 20 20 re f",
            ),
            // 9: /PInherit inherits /PageHatch from page /Resources
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>",
                b"/Pattern cs /PageHatch scn 0 0 20 20 re f",
            ),
            // 10: /PSelf is completely self-contained
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
                b"0 1 0 rg 0 0 5 10 re f",
            ),
            // 11: /PageHatch 10×10 stripe pattern anchored in page coordinates
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
                b"0 0 1 rg 0 0 5 10 re f",
            ),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let mut images = ImageCache::new();
        let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let (dl, _) = ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content);

        let outer_tilings: Vec<&std::sync::Arc<TilingPattern>> = dl
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                RenderCommand::FillPath {
                    paint: Paint::Tiling(t),
                    ..
                } => Some(t),
                _ => None,
            })
            .collect();
        // Order: [0] = /POut (2× Form), [1] = /PInherit@0, [2] = /PSelf@0, [3] = /PInherit@5, [4] = /PSelf@5
        assert_eq!(outer_tilings.len(), 5);

        let inner_tiling_matrix = |outer: &TilingPattern| -> Matrix {
            outer
                .commands
                .commands
                .iter()
                .find_map(|c| match c {
                    RenderCommand::FillPath {
                        paint: Paint::Tiling(inner),
                        ..
                    } => Some(inner.matrix),
                    _ => None,
                })
                .expect("expected nested Paint::Tiling inside outer cell")
        };

        // 1) Scaled 2× Form: outer matrix is 2×, inner matrix in cell space MUST be 1× (identity)
        //    so composed effective scale is 2× (not 4×).
        let pout = outer_tilings[0];
        assert!((pout.matrix.a - 2.0).abs() < 1e-6 && (pout.matrix.d - 2.0).abs() < 1e-6);
        let phatch_mat = inner_tiling_matrix(pout);
        assert!(
            (phatch_mat.a - 1.0).abs() < 1e-6 && (phatch_mat.d - 1.0).abs() < 1e-6,
            "inner Form-local hatch must be rebased to 1x in outer cell space, got {phatch_mat:?}"
        );

        // 2) Translated Form with page-anchored hatch:
        //    Placement 1 (dx=0): outer.e = 0, inner.e = 0 -> composed page origin = 0.
        //    Placement 2 (dx=5): outer.e = 5, inner.e = -5 -> composed page origin = 0!
        let pinherit_0 = outer_tilings[1];
        let pinherit_5 = outer_tilings[3];
        assert!(!std::sync::Arc::ptr_eq(
            &pinherit_0.commands,
            &pinherit_5.commands
        ));
        let inner_0 = inner_tiling_matrix(pinherit_0);
        let inner_5 = inner_tiling_matrix(pinherit_5);
        assert!(
            (inner_0.e - 0.0).abs() < 1e-6,
            "dx=0 placement inner page hatch offset must be 0, got {inner_0:?}"
        );
        assert!(
            (inner_5.e - (-5.0)).abs() < 1e-6,
            "dx=5 placement inner page hatch offset must be -5 so page anchor stays at 0, got {inner_5:?}"
        );
        let composed_5 = pinherit_5.matrix.concat(&inner_5);
        assert!(
            composed_5.e.abs() < 1e-6 && composed_5.f.abs() < 1e-6,
            "composed page-anchored hatch transform must stay anchored at page origin, got {composed_5:?}"
        );

        // 3) Self-contained /PSelf in the exact same two translated Forms MUST still hit cache!
        let pself_0 = outer_tilings[2];
        let pself_5 = outer_tilings[4];
        assert!(
            std::sync::Arc::ptr_eq(&pself_0.commands, &pself_5.commands),
            "self-contained Form-local tile must still share cached cell DisplayList across translations"
        );
    }

    #[test]
    fn anisotropic_clipped_dashed_mixed_curve_and_closed_join_and_magnified_tile() {
        let collect_contours = |path: &Path| -> Vec<Vec<Point>> {
            let mut contours: Vec<Vec<Point>> = Vec::new();
            for elem in path {
                match elem {
                    PathElement::MoveTo(p) => contours.push(vec![p]),
                    PathElement::LineTo(p) => {
                        if let Some(c) = contours.last_mut() {
                            c.push(p);
                        }
                    }
                    PathElement::CurveTo(c1, c2, end) => {
                        if let Some(c) = contours.last_mut() {
                            c.extend_from_slice(&[c1, c2, end]);
                        }
                    }
                    PathElement::Close => {}
                }
            }
            contours
        };
        let contour_bounds = |pts: &[Point]| -> (f64, f64, f64, f64) {
            let (mut x0, mut y0, mut x1, mut y1) = (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            );
            for p in pts {
                x0 = x0.min(p.x);
                y0 = y0.min(p.y);
                x1 = x1.max(p.x);
                y1 = y1.max(p.y);
            }
            (x0, y0, x1, y1)
        };

        // 1) Mixed compound path: 4x1 CTM, 2-unit pen, [5 5] 0 d, long off-page
        //    horizontal line PLUS an off-page cubic subpath. Must NOT fall back
        //    to StrokePath (which would turn 20pt x 2pt dashes into 12.5pt x 5pt).
        let mixed_stream = b"q 4 0 0 1 0 0 cm 2 w [5 5] 0 d \
                             -1000000 30 m 1000000 30 l \
                             -1000000 -500 m -999990 -480 -999980 -480 -999970 -500 c S Q";
        let (mixed_dl, _) = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 60.0))
            .interpret_with_stats(mixed_stream);
        assert!(
            !mixed_dl
                .commands
                .iter()
                .any(|c| matches!(c, RenderCommand::StrokePath { .. })),
            "off-page cubic subpath must not force anisotropic dashed stroke to StrokePath fallback"
        );
        let mixed_path = mixed_dl
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath { path, .. } => Some(path),
                _ => None,
            })
            .expect("mixed path must expand to FillPath");
        let mixed_boxes: Vec<(f64, f64, f64, f64)> = collect_contours(mixed_path)
            .iter()
            .map(|c| contour_bounds(c))
            .filter(|&(_, y0, _, y1)| y1 >= 0.0 && y0 <= 60.0)
            .collect();
        assert!(
            mixed_boxes.len() >= 3,
            "expected >= 3 visible horizontal dashes, got {mixed_boxes:?}"
        );
        for &(x0, y0, x1, y1) in &mixed_boxes {
            let w = x1 - x0;
            let h = y1 - y0;
            assert!(
                (h - 2.0).abs() < 0.05,
                "4x1 horizontal dash thickness must stay 2pt (not averaged 5pt), got {h}"
            );
            assert!(
                (w - 20.0).abs() < 0.1 || x0 <= 0.5 || x1 >= 99.5,
                "4x1 interior horizontal dash length must stay 20pt (not 12.5pt), got {w} in [{x0}..{x1}]"
            );
        }

        // Also test a huge off-page cubic subpath that itself exceeds MAX_DASH_REPEATS
        // alongside the visible horizontal line: must cull off-page cubic and keep 20pt x 2pt dashes.
        let huge_cubic_stream = b"q 4 0 0 1 0 0 cm 2 w [5 5] 0 d \
                                  -1000000 30 m 1000000 30 l \
                                  -1000000 -500 m -500000 -480 500000 -480 1000000 -500 c S Q";
        let (huge_cubic_dl, _) = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 60.0))
            .interpret_with_stats(huge_cubic_stream);
        assert!(
            !huge_cubic_dl
                .commands
                .iter()
                .any(|c| matches!(c, RenderCommand::StrokePath { .. })),
            "huge off-page cubic subpath must not force StrokePath fallback"
        );
        let huge_cubic_path = huge_cubic_dl
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath { path, .. } => Some(path),
                _ => None,
            })
            .expect("huge off-page cubic compound path must expand to FillPath");
        let huge_boxes: Vec<(f64, f64, f64, f64)> = collect_contours(huge_cubic_path)
            .iter()
            .map(|c| contour_bounds(c))
            .collect();
        assert!(
            huge_boxes.len() >= 3
                && huge_boxes
                    .iter()
                    .all(|&(_, y0, _, y1)| (y1 - y0 - 2.0).abs() < 0.05),
            "culled off-page cubic must leave only 2pt-tall visible dashes, got {huge_boxes:?}"
        );

        // Also test a SINGLE subpath containing an off-page cubic followed by a
        // long line crossing the page AND a visible cubic!
        let single_curve_stream = b"q 4 0 0 1 0 0 cm 2 w [5 5] 0 d \
                                    -1000000 30 m -999995 30 -999990 30 -999980 30 c \
                                    10 30 l 12 40 16 40 18 30 c 1000000 30 l S Q";
        let (single_dl, _) = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 60.0))
            .interpret_with_stats(single_curve_stream);
        assert!(
            single_dl
                .commands
                .iter()
                .any(|c| matches!(c, RenderCommand::FillPath { .. }))
                && !single_dl
                    .commands
                    .iter()
                    .any(|c| matches!(c, RenderCommand::StrokePath { .. })),
            "single subpath mixing long off-page lines and cubics must expand anisotropically without StrokePath fallback"
        );

        // 2a) Compound path: long off-page subpath + fully visible closed
        //     rectangle whose dash crosses the starting corner (5, 10) -> page (20, 10).
        //     Under 4x1, width 2, butt caps (0 J), miter join (0 j), [15 5] 5 d:
        //     the miter corner extends to page (16, 9) and must stay ONE joined
        //     contour covering x=16, y=9 rather than two butt caps missing (16..20, 9..10).
        let closed_compound = b"q 4 0 0 1 0 0 cm 2 w 0 J 0 j [15 5] 5 d \
                                -1000000 200 m 1000000 200 l \
                                5 10 10 20 re S Q";
        let (closed_dl, _) = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 60.0))
            .interpret_with_stats(closed_compound);
        let closed_path = closed_dl
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath { path, .. } => Some(path),
                _ => None,
            })
            .expect("compound closed rectangle must expand to FillPath");
        let has_miter_corner = |path: &Path| {
            collect_contours(path).iter().any(|pts| {
                pts.iter()
                    .any(|p| (p.x - 16.0).abs() < 0.05 && (p.y - 9.0).abs() < 0.05)
            })
        };
        assert!(
            has_miter_corner(closed_path),
            "closed rectangle alongside long off-page subpath must preserve miter join at (16, 9)"
        );

        // 2b) Single partially-clipped closed subpath starting at visible corner
        //     (5, 10) -> page (20, 10) and detouring off-page to x=1_000_005:
        //     perimeter = 2_000_040 (exact multiple of period 20), so the closing
        //     dash meets the starting dash at (20, 10) and must form a miter join
        //     reaching (16, 9)!
        let closed_detour = b"q 4 0 0 1 0 0 cm 2 w 0 J 0 j [15 5] 5 d \
                              5 10 m 1000005 10 l 1000005 30 l 5 30 l h S Q";
        let (detour_dl, _) = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 60.0))
            .interpret_with_stats(closed_detour);
        let detour_path = detour_dl
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath { path, .. } => Some(path),
                _ => None,
            })
            .expect("partially-clipped closed contour must expand to FillPath");
        assert!(
            has_miter_corner(detour_path),
            "partially-clipped closed contour whose dash crosses its starting corner must keep its miter join at (16, 9)"
        );

        // 3) Fine dashes inside a 100,000x magnified tiling pattern under a 4x1 cm:
        //    cell-space horizontal dash period is 0.00008 (<= 1e-4), which becomes
        //    8pt (4pt on, 4pt off) in page space under the 100,000x pattern matrix.
        let stream_obj = |dict: &str, data: &[u8]| -> Vec<u8> {
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
        let mag_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 80] /Contents 4 0 R \
              /Resources << /Pattern << /PMag 5 0 R >> >> >>"
                .to_vec(),
            stream_obj("", b"/Pattern cs /PMag scn 0 0 80 80 re f"),
            stream_obj(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /Matrix [100000 0 0 100000 0 0] \
                 /BBox [0 0 0.0004 0.0004] /XStep 0.0004 /YStep 0.0004 /Resources << >>",
                b"q 4 0 0 1 0 0 cm 1 0 0 RG 0.00002 w [0.00001 0.00001] 0 d \
                  0 0.0002 m 0.0001 0.0002 l S Q",
            ),
        ]);
        let doc = zpdf_document::PdfDocument::open(mag_pdf).expect("open mag pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let mut images = ImageCache::new();
        let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let (mag_dl, _) = ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content);
        let mag_tile = mag_dl
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath {
                    paint: Paint::Tiling(t),
                    ..
                } => Some(t),
                _ => None,
            })
            .expect("expected magnified Paint::Tiling");
        let cell_dash_path = mag_tile
            .commands
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath { path, .. } => Some(path),
                _ => None,
            })
            .expect(
                "anisotropic dashed stroke inside 100,000x magnified tile must expand to FillPath",
            );
        let cell_dashes: Vec<(f64, f64, f64, f64)> = collect_contours(cell_dash_path)
            .iter()
            .map(|c| contour_bounds(c))
            .collect();
        assert_eq!(
            cell_dashes.len(),
            5,
            "0.0001-user-unit line under 4x1 cm with [0.00001 0.00001] dash inside 100,000x tile must emit 5 separate dashes, got {cell_dashes:?}"
        );
        for &(x0, y0, x1, y1) in &cell_dashes {
            let page_w = (x1 - x0) * 100_000.0;
            let page_h = (y1 - y0) * 100_000.0;
            assert!(
                (page_w - 4.0).abs() < 0.05 && (page_h - 2.0).abs() < 0.05,
                "magnified tile dash must measure 4pt x 2pt in page space, got {page_w} x {page_h}"
            );
        }
    }

    #[test]
    fn type3_standalone_replay_draws_shallow_form_image_after_deep_alias() {
        // Standalone Type 3 CharProc replay (without a live PdfFile) using the
        // pre-resolved Type3Resources from font_loader: when /Resources visits
        // an 8-deep Form chain to a shared Form BEFORE a direct shallow alias
        // to that same Form, replaying the glyph that invokes the shallow alias
        // must still resolve and emit the Form's image XObject.
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),         // 1
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(), // 2
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 20 20] /Contents 4 0 R \
               /Resources << /Font << /F1 5 0 R >> >> >>"
                .into(), // 3
            "<< /Length 26 >>\nstream\nBT /F1 1 Tf (A) Tj ET\nendstream".into(), // 4
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] \
               /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R >> \
               /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [10] \
               /Resources << /XObject << /DeepChain 7 0 R /Shallow 15 0 R >> >> >>"
                .into(), // 5
            "<< /Length 19 >>\nstream\n10 0 d0 /Shallow Do\nendstream".into(), // 6
        ];
        // Objects 7..=14: 8-deep chain reaching shared Form 15 at depth 8.
        for obj_num in 7..=14u32 {
            let next = obj_num + 1;
            objs.push(format!(
                "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] \
                   /Resources << /XObject << /Next {next} 0 R >> >> /Length 8 >>\n\
                 stream\n/Next Do\nendstream"
            ));
        }
        // Object 15: shared Form defining /LeafImg (obj 16).
        objs.push(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] \
               /Resources << /XObject << /LeafImg 16 0 R >> >> /Length 27 >>\n\
             stream\n10 0 0 10 0 0 cm /LeafImg Do\nendstream"
                .into(),
        );
        // Object 16: 2x2 opaque red RGB image.
        objs.push(
            "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 \
               /BitsPerComponent 8 /ColorSpace /DeviceRGB /Length 12 >>\n\
             stream\n\x7f\x00\x00\x7f\x00\x00\x7f\x00\x00\x7f\x00\x00\nendstream"
                .into(),
        );

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objs.len());
        for (i, body) in objs.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            pdf.extend_from_slice(body.as_bytes());
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let fonts = doc.load_page_fonts(&page);
        let (_, font) = fonts
            .get_by_name("F1")
            .expect("Type 3 font F1 must be loaded");
        let (_, charproc_bytes, _) = font.type3_glyph_entry(65).expect("glyph A");
        let t3_res = font
            .type3_resources()
            .expect("Type 3 font must retain /Resources");
        let mut images = ImageCache::new();
        let (replay_dl, _) = ContentInterpreter::new(Rect::new(0.0, 0.0, 10.0, 10.0))
            .with_images(&mut images)
            .with_type3_resources(Some(t3_res))
            .for_type3_glyph()
            .interpret_with_stats(&charproc_bytes);
        assert!(
            replay_dl
                .commands
                .iter()
                .any(|c| matches!(c, RenderCommand::DrawImage(_) | RenderCommand::DrawInlineImage(_))),
            "standalone Type 3 replay of /Shallow Do must emit the image from Form 15's /Resources, got {:?}",
            replay_dl.commands
        );
    }

    #[test]
    fn anisotropic_stroke_point_budget_bounds_cumulative_page_and_cell_outlines() {
        // Each 4x1 dashed line `[2 2] 0 d` from x=0..20 produces 5 rectangular
        // dash contours (~20 points each). With a shared 60-point page allowance:
        // - Individual strokes are well within the limit (20 <= 60),
        // - Repeating the stroke 6 times (including inside a tiling cell) must
        //   expand only while within budget and fall back to StrokePath (with
        //   its dash array preserved) once the shared allowance is reached.
        let stream_obj = |dict: &str, data: &[u8]| -> Vec<u8> {
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

        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 120] /Contents 4 0 R \
              /Resources << /Pattern << /P1 5 0 R >> >> >>"
                .to_vec(),
            stream_obj(
                "",
                b"/Pattern cs /P1 scn 0 0 40 40 re f \
                  q 4 0 0 1 0 0 cm 2 w [2 2] 0 d \
                  0 50 m 20 50 l S \
                  0 60 m 20 60 l S \
                  0 70 m 20 70 l S \
                  0 80 m 20 80 l S \
                  0 90 m 20 90 l S Q",
            ),
            // Pattern cell itself expands a 4x1 dashed stroke (~20 pts) against the shared allowance.
            stream_obj(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 100 20] /XStep 100 /YStep 20 /Resources << >>",
                b"q 4 0 0 1 0 0 cm 2 w [2 2] 0 d 0 10 m 20 10 l S Q",
            ),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let allowance = 60usize;
        let (dl, _) = ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_anisotropic_stroke_point_limit(allowance)
            .interpret_with_stats(&content);

        let mut expanded_fill_count = 0usize;
        let mut fallback_stroke_count = 0usize;
        let mut total_expanded_pts = 0usize;
        for cmd in &dl.commands {
            match cmd {
                RenderCommand::FillPath {
                    path,
                    paint: Paint::Solid(_),
                    ..
                } => {
                    expanded_fill_count += 1;
                    total_expanded_pts += path.points().len();
                }
                RenderCommand::FillPath {
                    paint: Paint::Tiling(t),
                    ..
                } => {
                    for inner in &t.commands.commands {
                        if let RenderCommand::FillPath { path, .. } = inner {
                            expanded_fill_count += 1;
                            total_expanded_pts += path.points().len();
                        }
                    }
                }
                RenderCommand::StrokePath { style, .. } => {
                    assert!(
                        style.dash.is_some(),
                        "budget-exceeded fallback StrokePath must preserve dash pattern"
                    );
                    fallback_stroke_count += 1;
                }
                _ => {}
            }
        }
        assert!(
            expanded_fill_count >= 2,
            "first strokes within budget (including cell) must expand, got {expanded_fill_count}"
        );
        assert!(
            fallback_stroke_count >= 2,
            "later strokes past cumulative allowance must fall back to StrokePath, got {fallback_stroke_count}"
        );
        assert!(
            total_expanded_pts <= allowance,
            "cumulative expanded outline points ({total_expanded_pts}) must not exceed allowance ({allowance})"
        );
    }

    #[test]
    fn shading_pattern_falls_back_to_solid_average_when_image_budget_declines() {
        let stream_obj = |dict: &str, data: &[u8]| -> Vec<u8> {
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

        // Axial red-to-blue shading pattern applied to:
        // 1) a fill, 2) an isotropic stroke, and 3) a 4x1 anisotropic stroke.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
              /Resources << /Pattern << /PSh 5 0 R >> >> >>"
                .to_vec(),
            stream_obj(
                "",
                b"/Pattern cs /PSh scn 10 10 30 30 re f \
                  /Pattern CS /PSh SCN 4 w 10 60 m 90 60 l S \
                  q 4 0 0 1 0 0 cm 2 w 2 80 m 20 80 l S Q",
            ),
            b"<< /Type /Pattern /PatternType 2 /Shading 6 0 R >>".to_vec(),
            b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] \
              /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> \
              /Extend [true true] >>"
                .to_vec(),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf).expect("open");
        let page = doc.page(0).expect("page 0");
        let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");

        // Case 1: ImageCache budget (16 bytes) below gradient raster size.
        let mut tiny_images = ImageCache::new();
        let tiny_limits = zpdf_core::ParseLimits {
            max_image_cache_bytes: 16,
            ..Default::default()
        };
        let (dl_budget, _) = ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_images(&mut tiny_images)
            .with_limits(&tiny_limits)
            .interpret_with_stats(&content);

        // Case 2: Images disabled / no ImageCache.
        let (dl_no_img, _) = ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .interpret_with_stats(&content);

        for (label, dl) in [("tiny_budget", &dl_budget), ("no_images", &dl_no_img)] {
            let mut clip_balance = 0i32;
            let mut solid_fills = 0usize;
            let mut solid_strokes = 0usize;
            for cmd in &dl.commands {
                match cmd {
                    RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                        clip_balance += 1
                    }
                    RenderCommand::PopClip => clip_balance -= 1,
                    RenderCommand::FillPath {
                        paint: Paint::Solid(c),
                        ..
                    } => {
                        assert!(
                            c.r > 0.2 || c.b > 0.2,
                            "{label}: shading fill fallback must use gradient average color, got {c:?}"
                        );
                        solid_fills += 1;
                    }
                    RenderCommand::StrokePath {
                        paint: Paint::Solid(c),
                        ..
                    } => {
                        assert!(
                            c.r > 0.2 || c.b > 0.2,
                            "{label}: shading stroke fallback must use gradient average color, got {c:?}"
                        );
                        solid_strokes += 1;
                    }
                    RenderCommand::DrawImage(_) | RenderCommand::DrawInlineImage(_) => {
                        panic!("{label}: declined shading must not emit an image command");
                    }
                    _ => {}
                }
            }
            assert_eq!(clip_balance, 0, "{label}: clips must stay balanced");
            // 1 fill + 1 expanded anisotropic stroke fill = 2 FillPath, plus 1 isotropic StrokePath.
            assert_eq!(
                solid_fills, 2,
                "{label}: fill and anisotropic stroke must both fall back to visible solid FillPath"
            );
            assert_eq!(
                solid_strokes, 1,
                "{label}: isotropic stroke must fall back to visible solid StrokePath"
            );
        }
    }

    #[test]
    fn tiling_cell_cache_distinguishes_type3_inline_image_embedding_in_both_orders() {
        let stream_obj = |dict: &str, data: &[u8]| -> Vec<u8> {
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

        // Page-level tiling pattern whose cell paints a 1x1 red inline RGB image.
        // Without an external ImageCache:
        // - ordinary page fills omit inline images;
        // - active Type 3 glyphs embed them via `DrawInlineImage`.
        // Test BOTH paint orders so neither poisons `tiling_cell_cache` for the other.
        let run_order = |page_stream: &[u8]| -> (bool, bool) {
            let mut pat_cell = b"10 0 0 10 0 0 cm BI /W 1 /H 1 /CS /RGB /BPC 8 ID ".to_vec();
            pat_cell.extend_from_slice(&[255, 0, 0]);
            pat_cell.extend_from_slice(b" EI");
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 60] /Contents 4 0 R \
                  /Resources << /Font << /F1 5 0 R >> /Pattern << /PImg 8 0 R >> >> >>"
                    .to_vec(),
                stream_obj("", page_stream),
                // Type 3 font with NO /Resources of its own -> identical resource_scopes to page!
                b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] \
                  /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 7 0 R >> \
                  /Encoding 6 0 R /FirstChar 65 /LastChar 65 /Widths [10] >>"
                    .to_vec(),
                b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(),
                // d0 glyph inherits the page's active /PImg pattern and paints a rect.
                stream_obj("", b"10 0 d0 0 0 10 10 re f"),
                stream_obj(
                    "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                     /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
                    &pat_cell,
                ),
            ]);

            let doc = zpdf_document::PdfDocument::open(pdf).expect("open");
            let page = doc.page(0).expect("page 0");
            let mut fonts = doc.load_page_fonts(&page);
            let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
            // Live document, NO external ImageCache.
            let (dl, _) = ContentInterpreter::new(page.effective_box())
                .with_document(doc.file(), &page.resources)
                .with_fonts(&mut fonts)
                .interpret_with_stats(&content);

            let fills: Vec<(&Path, &std::sync::Arc<TilingPattern>)> = dl
                .commands
                .iter()
                .filter_map(|c| match c {
                    RenderCommand::FillPath {
                        path,
                        paint: Paint::Tiling(t),
                        ..
                    } => Some((path, t)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                fills.len(),
                2,
                "expected 1 page fill and 1 Type 3 glyph fill"
            );
            // Page fill is at x=[0..20], glyph fill is placed at x=[40..50].
            let mut page_has_inline = false;
            let mut glyph_has_inline = false;
            for (path, tiling) in fills {
                let x0 = path
                    .points()
                    .iter()
                    .map(|p| p.x)
                    .fold(f64::INFINITY, f64::min);
                let has_inline = tiling
                    .commands
                    .commands
                    .iter()
                    .any(|c| matches!(c, RenderCommand::DrawInlineImage(_)));
                if x0 < 30.0 {
                    page_has_inline = has_inline;
                } else {
                    glyph_has_inline = has_inline;
                }
            }
            (page_has_inline, glyph_has_inline)
        };

        let (page_first_page, page_first_glyph) =
            run_order(b"/Pattern cs /PImg scn 0 0 20 20 re f BT /F1 1 Tf 40 0 Td (A) Tj ET");
        assert!(
            !page_first_page,
            "page-first order: page fill without ImageCache must omit inline image"
        );
        assert!(
            page_first_glyph,
            "page-first order: Type 3 glyph must NOT reuse page cell missing inline image"
        );

        let (glyph_first_page, glyph_first_glyph) =
            run_order(b"/Pattern cs /PImg scn BT /F1 1 Tf 40 0 Td (A) Tj ET 0 0 20 20 re f");
        assert!(
            glyph_first_glyph,
            "glyph-first order: Type 3 glyph must embed inline image"
        );
        assert!(
            !glyph_first_page,
            "glyph-first order: page fill must NOT reuse Type 3 cell carrying DrawInlineImage"
        );
    }

    #[test]
    fn anisotropic_stroke_fallback_captures_table_rule_once() {
        let mut rules = Vec::new();
        // 4x horizontal, 1x vertical nonuniform CTM triggers anisotropic stroke
        // expansion; a zero point budget forces `AnisotropicStroke::Fallback`
        // into `emit_stroke`. The horizontal rule (page x=[40..240], y=20)
        // must be recorded exactly once.
        let stream = b"4 0 0 1 0 0 cm 1 w 10 20 m 60 20 l S";
        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 300.0, 100.0))
            .with_rule_sink(&mut rules)
            .with_anisotropic_stroke_point_limit(0)
            .interpret(stream);

        assert!(
            dl.commands
                .iter()
                .any(|c| matches!(c, RenderCommand::StrokePath { .. })),
            "zero point allowance must fall back to StrokePath"
        );
        assert_eq!(
            rules.len(),
            1,
            "anisotropic stroke fallback must record the table rule exactly once, got {rules:?}"
        );
        assert!(!rules[0].vertical);
        assert!((rules[0].pos - 20.0).abs() < 1e-6);
        assert!((rules[0].start - 40.0).abs() < 1e-6);
        assert!((rules[0].end - 240.0).abs() < 1e-6);
    }
}
