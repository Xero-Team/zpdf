use std::collections::HashMap;

use zpdf_core::{Matrix, ObjectId, PdfObject, Point, Rect};
use zpdf_display_list::*;
use zpdf_document::page::ResourceDict;
use zpdf_font::FontCache;
use zpdf_image::ImageCache;
use zpdf_parser::PdfFile;

use crate::text::TextSpan;
use crate::tokenizer::{ContentToken, ContentTokenizer};

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
    font_cache: Option<&'a mut FontCache>,
    current_font_id: Option<zpdf_font::FontId>,
    file: Option<&'a PdfFile>,
    resources: Option<&'a ResourceDict>,
    image_cache: Option<&'a mut ImageCache>,
    icc_cache: Option<&'a mut zpdf_color::IccCache>,
    /// Maps an image XObject's object id to its already-decoded entry in
    /// `image_cache`, so an image drawn many times (e.g. a map symbol repeated
    /// thousands of times) is decoded — colour-converted, soft-mask-folded — only
    /// once instead of per `Do`. Keyed only for stateless images; `/ImageMask`
    /// stencils bake in the current fill colour and are never cached here.
    image_obj_cache: HashMap<ObjectId, zpdf_display_list::ImageId>,
    /// Built shadings (256-entry LUT + parsed functions) cached by object id, so a
    /// shading painted many times (`sh` markers repeated across a map) rebuilds its
    /// LUT only once. Reuse overwrites `to_page` with the current CTM.
    shading_cache: HashMap<ObjectId, crate::shading::ShadingDef>,
    form_font_overrides: Vec<HashMap<String, String>>,
    /// Owned /Resources of the form XObjects currently being interpreted,
    /// innermost last. Lookups search these before the page resources.
    form_resources: Vec<ResourceDict>,
    form_depth: u32,
    /// `Q` must never pop the state stack below this index — a form XObject
    /// raises it past its own saved state so an unbalanced `Q` inside the
    /// form cannot corrupt the page-level state.
    state_floor: usize,
    text_sink: Option<&'a mut Vec<TextSpan>>,
    /// The CTM at page-content start (identity or the baked page rotation).
    /// Pattern space is anchored to this, not to the CTM at fill time.
    base_ctm: Matrix,
    /// Inside an uncolored (PaintType 2) tiling-pattern cell: color operators
    /// are ignored so the cell paints with the pattern's `scn` color.
    suppress_color_ops: bool,
    /// Default optional-content configuration; `None` renders everything.
    oc_config: Option<&'a zpdf_document::OcConfig>,
    /// Annotations painted after the page content (appearance streams).
    annotations: Option<&'a [zpdf_document::Annotation]>,
    /// Marked-content nesting depth (BMC/BDC vs EMC).
    mc_depth: u32,
    /// Depth at which a hidden `BDC /OC` block began; painting is suppressed
    /// until the matching EMC.
    oc_hidden_from: Option<u32>,
    /// Hard ceiling on emitted display-list commands. Adversarial or pathological
    /// content (huge form-XObject fanout, massive tiling-pattern replication,
    /// repeated shadings) can otherwise build a multi-million-command list that
    /// hangs the renderer. Once exceeded, the interpreter stops emitting and
    /// returns the partial page rather than hanging.
    max_commands: usize,
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
    deadline: Option<std::time::Instant>,
    /// Latches once `deadline` passes, so the clock is read only occasionally.
    timed_out: bool,
    /// `ops_executed` at the last clock sample (throttles `Instant::now`).
    last_clock_op: u64,
    /// Tiling-pattern-scoped soft-mask reuse, installed by
    /// `paint_tiling_pattern` for the duration of its tile loop (`None`
    /// elsewhere). Tiles replay identical content under CTMs that differ only
    /// by translation, so a mask built once at the canonical tile position is
    /// exact on every tile — see `build_or_reuse_soft_mask`.
    soft_mask_reuse: Option<TileMaskReuse>,
    /// Operator counter within the current tile replay, reset at each tile
    /// start. Part of the reuse key: the same count identifies the same `gs`
    /// site (hence identical inherited state) across tiles, and two distinct
    /// sites within one cell can never collide.
    tile_op_index: u64,
}

/// State for soft-mask reuse across the tiles of one `paint_tiling_pattern`
/// loop. Masks are built as if at the *canonical* tile — the middle of the
/// tile index range, whose cell lies inside the page window — and reused
/// everywhere else via [`SoftMask::offset`]. Backends shift the rasterized
/// plane by that page-space offset; samples are only ever taken inside the
/// painting tile's cell, whose pre-image is the canonical cell, which the
/// page-rect raster window covers.
struct TileMaskReuse {
    masks: HashMap<SoftMaskReuseKey, SoftMaskReuseEntry>,
    /// Page-space offset of the tile currently being replayed, relative to
    /// the canonical tile: `to_page_linear · ((i−ic)·XStep, (j−jc)·YStep)`.
    cur_delta: (f64, f64),
}

/// Key for tiling-pattern soft-mask reuse: a `gs` site (operator index within
/// the tile replay + ExtGState object) plus the CTM's linear part. A hit
/// means the cached mask differs from the needed one only by the CTM
/// translation delta, which maps 1:1 to a page-space offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SoftMaskReuseKey {
    op_index: u64,
    gs_id: ObjectId,
    ctm_linear: [u64; 4],
}

#[derive(Debug, Clone)]
struct SoftMaskReuseEntry {
    mask: SoftMask,
    /// CTM translation of the canonical-position build; reuse offsets are
    /// relative to this.
    build_e: f64,
    build_f: f64,
}

/// Wall-clock budget for interpreting a single page (see `deadline`). Generous
/// for any real page (which interpret in well under a second); bounds
/// adversarial shading/pattern blowups.
const INTERPRET_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// Default [`ContentInterpreter::max_commands`]. Real pages run to at most low
/// hundreds of thousands of commands, so this is a generous headroom while still
/// cutting off runaway form-XObject/tiling/shading emitters before their
/// emission *time* (not just memory) becomes a hang. Tuned together with the
/// renderer's wall-clock budget so interpret + render stay comfortably bounded.
const DEFAULT_MAX_COMMANDS: usize = 500_000;

/// Default [`ContentInterpreter::max_ops`]. Bounds total interpreter work
/// (operators executed) so exponential form/pattern fanout that emits few
/// commands per call still terminates promptly.
const DEFAULT_MAX_OPS: u64 = 4_000_000;

#[derive(Debug, Clone)]
enum ActiveColorSpace {
    DeviceGray,
    DeviceRGB,
    DeviceCMYK,
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
    /// Separation (n = 1) or DeviceN (n = components) with its tint transform
    /// and alternate space. A missing/unparseable transform falls back to
    /// `gray(1 - max(tint))`, which has the right polarity for colorants.
    Tint {
        n: usize,
        transform: Option<std::sync::Arc<zpdf_color::PdfFunction>>,
        alternate: Box<ActiveColorSpace>,
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
            Self::DeviceGray => 1,
            Self::DeviceRGB => 3,
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

/// Resolved pattern paint selected via `scn` in a Pattern colorspace.
#[derive(Debug, Clone)]
enum PatternPaint {
    Shading(std::sync::Arc<crate::shading::ShadingDef>),
    Tiling(std::sync::Arc<TilingPatternDef>),
}

/// A tiling pattern (PatternType 1), resolved at `scn` time and replicated
/// cell-by-cell at fill time.
#[derive(Debug)]
struct TilingPatternDef {
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
            fill_cs: ActiveColorSpace::DeviceGray,
            stroke_cs: ActiveColorSpace::DeviceGray,
            fill_pattern: None,
            stroke_pattern: None,
            blend_mode: BlendMode::Normal,
            soft_mask: None,
            rendering_intent: zpdf_color::RenderIntent::RelativeColorimetric,
        }
    }
}

impl<'a> ContentInterpreter<'a> {
    pub fn new(page_rect: Rect) -> Self {
        Self {
            state_stack: Vec::new(),
            current: GraphicsState::default(),
            display_list: DisplayList::new(page_rect),
            current_path: Path::new(),
            operand_stack: Vec::new(),
            text_active: false,
            text_matrix: Matrix::identity(),
            text_line_matrix: Matrix::identity(),
            font_cache: None,
            current_font_id: None,
            file: None,
            resources: None,
            image_cache: None,
            icc_cache: None,
            image_obj_cache: HashMap::new(),
            shading_cache: HashMap::new(),
            form_font_overrides: Vec::new(),
            form_resources: Vec::new(),
            form_depth: 0,
            state_floor: 0,
            text_sink: None,
            base_ctm: Matrix::identity(),
            suppress_color_ops: false,
            oc_config: None,
            annotations: None,
            mc_depth: 0,
            oc_hidden_from: None,
            max_commands: DEFAULT_MAX_COMMANDS,
            ops_executed: 0,
            max_ops: DEFAULT_MAX_OPS,
            deadline: None,
            timed_out: false,
            last_clock_op: 0,
            soft_mask_reuse: None,
            tile_op_index: 0,
        }
    }

    /// Override the emitted-command ceiling (see [`DEFAULT_MAX_COMMANDS`]).
    pub fn with_command_limit(mut self, max_commands: usize) -> Self {
        self.max_commands = max_commands;
        self
    }

    /// True once the page has hit any anti-hang ceiling: emitted commands,
    /// executed operators, or the wall-clock deadline. Checked by the top-level
    /// loop and the recursive form/tiling emitters so a runaway page is
    /// truncated rather than hanging — whether it explodes in emitted commands,
    /// raw call fanout, or per-call time (e.g. per-cell shading rasterization).
    /// The clock is sampled at most once per 256 operators to keep it cheap.
    fn over_budget(&mut self) -> bool {
        if self.display_list.commands.len() >= self.max_commands
            || self.ops_executed >= self.max_ops
        {
            return true;
        }
        if !self.timed_out {
            if let Some(deadline) = self.deadline {
                if self.ops_executed.wrapping_sub(self.last_clock_op) >= 16 {
                    self.last_clock_op = self.ops_executed;
                    if std::time::Instant::now() >= deadline {
                        self.timed_out = true;
                        tracing::warn!("content interpret exceeded time budget; truncating page");
                    }
                }
            }
        }
        self.timed_out
    }

    /// Look a value up through the form-resources stack (innermost first),
    /// falling back to the page-level resources.
    fn lookup_res<T>(&self, get: impl Fn(&ResourceDict) -> Option<T>) -> Option<T> {
        for r in self.form_resources.iter().rev() {
            if let Some(v) = get(r) {
                return Some(v);
            }
        }
        self.resources.and_then(get)
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
        // Matrix::new(a,b,c,d,e,f) maps (x,y) -> (a*x+c*y+e, b*x+d*y+f). Each
        // turn maps the W×H page into the rotated box with origin bottom-left.
        let (base, rotated) = match r {
            90 => (
                Matrix::new(0.0, -1.0, 1.0, 0.0, 0.0, w),
                Rect::new(0.0, 0.0, h, w),
            ),
            180 => (
                Matrix::new(-1.0, 0.0, 0.0, -1.0, w, h),
                Rect::new(0.0, 0.0, w, h),
            ),
            270 => (
                Matrix::new(0.0, 1.0, -1.0, 0.0, h, 0.0),
                Rect::new(0.0, 0.0, h, w),
            ),
            _ => return self,
        };
        self.current.ctm = base;
        self.base_ctm = base;
        self.display_list.page_rect = rotated;
        self
    }

    pub fn with_fonts(mut self, cache: &'a mut FontCache) -> Self {
        self.font_cache = Some(cache);
        self
    }

    /// Collect decoded text spans (for the `text` command / extraction) in addition
    /// to building the display list.
    pub fn with_text_sink(mut self, sink: &'a mut Vec<TextSpan>) -> Self {
        self.text_sink = Some(sink);
        self
    }

    pub fn with_document(mut self, file: &'a PdfFile, resources: &'a ResourceDict) -> Self {
        self.file = Some(file);
        self.resources = Some(resources);
        self
    }

    pub fn with_images(mut self, cache: &'a mut ImageCache) -> Self {
        self.image_cache = Some(cache);
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

    pub fn interpret(mut self, content: &[u8]) -> DisplayList {
        // Arm the interpret wall-clock backstop (unless already set by a caller).
        if self.deadline.is_none() {
            self.deadline = Some(std::time::Instant::now() + INTERPRET_BUDGET);
        }
        let tokenizer = ContentTokenizer::new(content);

        for token in tokenizer {
            if self.over_budget() {
                tracing::warn!(
                    "content exceeded {} display commands; truncating page",
                    self.max_commands
                );
                break;
            }
            match token {
                ContentToken::Operand(obj) => {
                    self.operand_stack.push(obj);
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

        // Annotation appearances render independently of wherever the page
        // content stream left the graphics state (12.5.5): rebalance any
        // unclosed q-levels and top-level W clips, then reset to the page's
        // initial state (an unbalanced BDC /OC must not blank annotations).
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
        self.oc_hidden_from = None;
        self.suppress_color_ops = false;

        self.paint_annotations();

        self.display_list
    }

    pub fn command_count(&self) -> usize {
        self.display_list.commands.len()
    }

    fn pop_f64(&mut self) -> f64 {
        self.operand_stack
            .pop()
            .and_then(|o| o.as_f64().ok())
            .unwrap_or(0.0)
    }

    fn pop_name(&mut self) -> String {
        self.operand_stack
            .pop()
            .and_then(|o| match o {
                PdfObject::Name(n) => Some(n.0),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn pop_string_bytes(&mut self) -> Vec<u8> {
        self.operand_stack
            .pop()
            .and_then(|o| match o {
                PdfObject::String(s) => Some(s.0),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn execute_operator(&mut self, op: &str) {
        // Count every operator toward the global work budget so exponential
        // form/pattern fanout (which emits few commands per call) still stops.
        self.ops_executed = self.ops_executed.saturating_add(1);

        // Position counter for tiling-pattern soft-mask reuse: the n-th
        // operator of tile k is the n-th operator of every other tile, so the
        // count identifies a `gs` site across tiles (including operators in
        // nested form XObjects, which replay identically per tile).
        if self.soft_mask_reuse.is_some() {
            self.tile_op_index += 1;
        }
        // Uncolored (PaintType 2) pattern cells paint exclusively with the
        // color given at `scn` time; their own color operators are ignored.
        if self.suppress_color_ops
            && matches!(
                op,
                "g" | "G" | "rg" | "RG" | "k" | "K" | "cs" | "CS" | "sc" | "scn" | "SC" | "SCN"
            )
        {
            return;
        }
        match op {
            // -- Graphics state --
            "q" => {
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
                    // clips counted above are gone.
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
                self.current.ctm = self.current.ctm.concat(&m);
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
                let y = self.pop_f64();
                let x = self.pop_f64();
                self.current_path.move_to(Point::new(x, y));
            }
            "l" => {
                let y = self.pop_f64();
                let x = self.pop_f64();
                self.current_path.line_to(Point::new(x, y));
            }
            "c" => {
                let y3 = self.pop_f64();
                let x3 = self.pop_f64();
                let y2 = self.pop_f64();
                let x2 = self.pop_f64();
                let y1 = self.pop_f64();
                let x1 = self.pop_f64();
                self.current_path.curve_to(
                    Point::new(x1, y1),
                    Point::new(x2, y2),
                    Point::new(x3, y3),
                );
            }
            "v" => {
                let y3 = self.pop_f64();
                let x3 = self.pop_f64();
                let y2 = self.pop_f64();
                let x2 = self.pop_f64();
                // v: current point is first control point
                self.current_path.curve_to(
                    self.current_point(),
                    Point::new(x2, y2),
                    Point::new(x3, y3),
                );
            }
            "y" => {
                let y3 = self.pop_f64();
                let x3 = self.pop_f64();
                let y1 = self.pop_f64();
                let x1 = self.pop_f64();
                // y: end point is second control point
                self.current_path.curve_to(
                    Point::new(x1, y1),
                    Point::new(x3, y3),
                    Point::new(x3, y3),
                );
            }
            "h" => self.current_path.close(),
            "re" => {
                let h = self.pop_f64();
                let w = self.pop_f64();
                let y = self.pop_f64();
                let x = self.pop_f64();
                self.current_path.rect(Rect::new(x, y, x + w, y + h));
            }

            // -- Path painting --
            "S" => self.paint_stroke(),
            "s" => {
                self.current_path.close();
                self.paint_stroke();
            }
            "f" | "F" => self.paint_fill(FillRule::NonZero),
            "f*" => self.paint_fill(FillRule::EvenOdd),
            "B" => {
                self.paint_fill_then_stroke(FillRule::NonZero);
            }
            "B*" => {
                self.paint_fill_then_stroke(FillRule::EvenOdd);
            }
            "b" => {
                self.current_path.close();
                self.paint_fill_then_stroke(FillRule::NonZero);
            }
            "b*" => {
                self.current_path.close();
                self.paint_fill_then_stroke(FillRule::EvenOdd);
            }
            "n" => {
                self.current_path = Path::new();
            }

            // -- Clipping --
            "W" => {
                let path = self.transform_path_to_page_space(&self.current_path.clone());
                self.intersect_clip_bounds(Self::path_bounds(&path));
                self.display_list.push(RenderCommand::PushClip {
                    path,
                    rule: FillRule::NonZero,
                });
                self.current.clip_depth += 1;
            }
            "W*" => {
                let path = self.transform_path_to_page_space(&self.current_path.clone());
                self.intersect_clip_bounds(Self::path_bounds(&path));
                self.display_list.push(RenderCommand::PushClip {
                    path,
                    rule: FillRule::EvenOdd,
                });
                self.current.clip_depth += 1;
            }

            // -- Color --
            "g" => {
                let gray = self.pop_f64() as f32;
                self.current.fill_color = Color::gray(gray);
                self.current.fill_cs = ActiveColorSpace::DeviceGray;
                self.current.fill_pattern = None;
            }
            "G" => {
                let gray = self.pop_f64() as f32;
                self.current.stroke_color = Color::gray(gray);
                self.current.stroke_cs = ActiveColorSpace::DeviceGray;
                self.current.stroke_pattern = None;
            }
            "rg" => {
                let b = self.pop_f64() as f32;
                let g = self.pop_f64() as f32;
                let r = self.pop_f64() as f32;
                self.current.fill_color = Color::rgb(r, g, b);
                self.current.fill_cs = ActiveColorSpace::DeviceRGB;
                self.current.fill_pattern = None;
            }
            "RG" => {
                let b = self.pop_f64() as f32;
                let g = self.pop_f64() as f32;
                let r = self.pop_f64() as f32;
                self.current.stroke_color = Color::rgb(r, g, b);
                self.current.stroke_cs = ActiveColorSpace::DeviceRGB;
                self.current.stroke_pattern = None;
            }
            "k" => {
                let k_val = self.pop_f64();
                let y_val = self.pop_f64();
                let m_val = self.pop_f64();
                let c_val = self.pop_f64();
                let (r, g, b) = zpdf_color::cmyk_to_rgb(c_val, m_val, y_val, k_val);
                self.current.fill_color = Color::rgb(r as f32, g as f32, b as f32);
                self.current.fill_cs = ActiveColorSpace::DeviceCMYK;
                self.current.fill_pattern = None;
            }
            "K" => {
                let k_val = self.pop_f64();
                let y_val = self.pop_f64();
                let m_val = self.pop_f64();
                let c_val = self.pop_f64();
                let (r, g, b) = zpdf_color::cmyk_to_rgb(c_val, m_val, y_val, k_val);
                self.current.stroke_color = Color::rgb(r as f32, g as f32, b as f32);
                self.current.stroke_cs = ActiveColorSpace::DeviceCMYK;
                self.current.stroke_pattern = None;
            }
            "cs" => {
                let name = self.pop_name();
                self.current.fill_cs = self.resolve_color_space(&name);
                self.current.fill_pattern = None;
                // Per spec, cs resets the color to the space's initial value
                // (black / index 0 / tint 1.0 all paint-as-dark; Pattern has none).
                if !matches!(self.current.fill_cs, ActiveColorSpace::Pattern { .. }) {
                    self.current.fill_color = self.initial_color(&self.current.fill_cs);
                }
            }
            "CS" => {
                let name = self.pop_name();
                self.current.stroke_cs = self.resolve_color_space(&name);
                self.current.stroke_pattern = None;
                if !matches!(self.current.stroke_cs, ActiveColorSpace::Pattern { .. }) {
                    self.current.stroke_color = self.initial_color(&self.current.stroke_cs);
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
                } else {
                    let cs = self.current.fill_cs.clone();
                    self.current.fill_color = self.pop_color(&cs);
                }
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
                } else {
                    let cs = self.current.stroke_cs.clone();
                    self.current.stroke_color = self.pop_color(&cs);
                }
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
            }
            "ET" => {
                self.text_active = false;
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
                self.show_text(&bytes);
            }
            "TJ" => {
                if let Some(PdfObject::Array(arr)) = self.operand_stack.pop() {
                    for item in arr {
                        match item {
                            PdfObject::String(s) => self.show_text(&s.0),
                            PdfObject::Integer(n) => {
                                self.adjust_text_position(-n as f64);
                            }
                            PdfObject::Real(n) => {
                                self.adjust_text_position(-n);
                            }
                            _ => {}
                        }
                    }
                }
            }
            "'" => {
                // Move to next line and show text
                let leading = self.current.leading as f64;
                let translate = Matrix::translate(0.0, -leading);
                self.text_line_matrix = self.text_line_matrix.concat(&translate);
                self.text_matrix = self.text_line_matrix;
                let bytes = self.pop_string_bytes();
                self.show_text(&bytes);
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
                self.show_text(&bytes);
            }

            // -- XObject --
            "Do" => {
                let name = self.pop_name();
                self.execute_do(&name);
            }

            // -- Type3 glyph operators --
            "d0" => {
                // wx wy d0: set glyph width (2 operands, consume them)
            }
            "d1" => {
                // wx wy llx lly urx ury d1: set glyph width and bbox (6 operands)
            }

            // -- Marked content --
            "BMC" => {
                self.mc_depth += 1;
            }
            "BDC" => {
                // Operands: tag (first) then properties (top of stack).
                let props = self.operand_stack.pop();
                let tag = match self.operand_stack.pop() {
                    Some(PdfObject::Name(n)) => n.0,
                    _ => String::new(),
                };
                self.mc_depth += 1;
                if tag == "OC" && self.oc_hidden_from.is_none() {
                    let visible = props
                        .map(|p| self.oc_properties_visible(&p))
                        .unwrap_or(true);
                    if !visible {
                        self.oc_hidden_from = Some(self.mc_depth);
                    }
                }
            }
            "EMC" => {
                if self.oc_hidden_from == Some(self.mc_depth) {
                    self.oc_hidden_from = None;
                }
                self.mc_depth = self.mc_depth.saturating_sub(1);
            }
            "MP" | "DP" => {}

            _ => {}
        }
    }

    fn resolve_color_space(&mut self, name: &str) -> ActiveColorSpace {
        match name {
            "DeviceGray" | "G" => ActiveColorSpace::DeviceGray,
            "DeviceRGB" | "RGB" => ActiveColorSpace::DeviceRGB,
            "DeviceCMYK" | "CMYK" => ActiveColorSpace::DeviceCMYK,
            "Pattern" => ActiveColorSpace::Pattern { base: None },
            _ => {
                // Resource lookup: direct (inline) values first, then refs.
                if let Some(obj) = self.lookup_res(|r| r.color_spaces_inline.get(name).cloned()) {
                    return self.resolve_color_space_obj(&obj, 0);
                }
                let cs_id = match self.lookup_res(|r| r.color_spaces.get(name).copied()) {
                    Some(id) => id,
                    None => return ActiveColorSpace::DeviceGray,
                };
                let obj = match self.file.map(|f| f.resolve(cs_id)) {
                    Some(Ok(o)) => o,
                    _ => return ActiveColorSpace::DeviceGray,
                };
                self.resolve_color_space_obj(&obj, 0)
            }
        }
    }

    /// Resolve a colorspace object (name or array) into an evaluatable space.
    fn resolve_color_space_obj(&mut self, obj: &PdfObject, depth: u8) -> ActiveColorSpace {
        if depth > 4 {
            return ActiveColorSpace::DeviceGray;
        }
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
            PdfObject::Name(n) => return self.resolve_color_space(&n.0),
            PdfObject::Array(arr) => arr,
            _ => return ActiveColorSpace::DeviceGray,
        };
        let Some(PdfObject::Name(cs_name)) = arr.first() else {
            return ActiveColorSpace::DeviceGray;
        };
        match cs_name.as_str() {
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
                let n = if cs_name.as_str() == "Separation" {
                    1
                } else {
                    match arr.get(1).map(|o| self.resolve_plain(o)) {
                        Some(PdfObject::Array(names)) => names.len().max(1),
                        _ => 1,
                    }
                };
                let alternate = arr
                    .get(2)
                    .map(|o| self.resolve_color_space_obj(o, depth + 1))
                    .unwrap_or(ActiveColorSpace::DeviceGray);
                let transform = arr.get(3).and_then(|o| self.parse_function(o));
                ActiveColorSpace::Tint {
                    n,
                    transform: transform.map(std::sync::Arc::new),
                    alternate: Box::new(alternate),
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
                            if let Ok(x) = v.as_f64() {
                                range[i] = x;
                            }
                        }
                    }
                }
                ActiveColorSpace::Lab { white_point, range }
            }
            "CalGray" => ActiveColorSpace::DeviceGray,
            "CalRGB" => ActiveColorSpace::DeviceRGB,
            // `[/Pattern base]` — retain the base space for uncolored patterns.
            "Pattern" => ActiveColorSpace::Pattern {
                base: arr
                    .get(1)
                    .map(|o| Box::new(self.resolve_color_space_obj(o, depth + 1))),
            },
            _ => ActiveColorSpace::DeviceGray,
        }
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
            PdfObject::Stream(s) => build_inline_icc_transform(s, intent),
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

    fn pop_color(&mut self, cs: &ActiveColorSpace) -> Color {
        let n = cs.components();
        let mut vals = Vec::with_capacity(n);
        for _ in 0..n {
            vals.push(self.pop_f64());
        }
        vals.reverse();
        self.components_to_rgb(cs, &vals)
    }

    /// Convert component values in `cs` to an RGB display color.
    fn components_to_rgb(&self, cs: &ActiveColorSpace, vals: &[f64]) -> Color {
        let get = |i: usize| vals.get(i).copied().unwrap_or(0.0);
        match cs {
            ActiveColorSpace::DeviceGray | ActiveColorSpace::ICCBased(1) => {
                Color::gray(get(0) as f32)
            }
            ActiveColorSpace::DeviceRGB | ActiveColorSpace::ICCBased(3) => {
                Color::rgb(get(0) as f32, get(1) as f32, get(2) as f32)
            }
            ActiveColorSpace::DeviceCMYK | ActiveColorSpace::ICCBased(4) => {
                let (r, g, b) = zpdf_color::cmyk_to_rgb(get(0), get(1), get(2), get(3));
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

    /// Resolve a pattern name selected via `scn`/`SCN`. Returns the pattern
    /// paint (if usable) plus a solid approximation color for paths that
    /// cannot take the real paint (e.g. shading-pattern strokes).
    fn resolve_pattern(&mut self, name: &str) -> (Option<PatternPaint>, Color) {
        let Some(file) = self.file else {
            return (None, Color::gray(0.5));
        };
        let Some(pat_id) = self.lookup_res(|r| r.patterns.get(name).copied()) else {
            tracing::debug!("pattern {name} not found in resources");
            return (None, Color::gray(0.5));
        };
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
            if let Some(sh_obj) = dict.get("Shading") {
                // The pattern /Matrix maps pattern space to the page's *default*
                // user space, so the shading→page transform is base_ctm · matrix
                // (mirrors the tiling path). Without base_ctm a shading pattern
                // ignores page /Rotate and any enclosing form CTM.
                if let Some(def) = self.build_shading(sh_obj, self.base_ctm.concat(&matrix)) {
                    let avg = def.average_rgb();
                    let approx = Color::rgb(avg[0], avg[1], avg[2]);
                    return (
                        Some(PatternPaint::Shading(std::sync::Arc::new(def))),
                        approx,
                    );
                }
            }
            return (None, Color::gray(0.5));
        }

        // Tiling pattern (PatternType 1): capture everything needed to replay
        // the cell at fill time. The gray approximation remains the fallback
        // for paths that cannot take the real paint (e.g. strokes).
        let PdfObject::Stream(stream) = &obj else {
            return (None, Color::gray(0.5));
        };
        let content = match file
            .resolve_stream_data(pat_id)
            .or_else(|_| zpdf_parser::filters::decode_stream(&stream.data, &stream.dict))
        {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!("tiling pattern {name}: undecodable stream - {e}");
                return (None, Color::gray(0.5));
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
            return (None, Color::gray(0.5));
        };
        // Steps default to the cell size; zero/non-finite steps are degenerate.
        let x_step = dict.get_f64("XStep").unwrap_or(bbox.width());
        let y_step = dict.get_f64("YStep").unwrap_or(bbox.height());
        if !x_step.is_finite() || !y_step.is_finite() || x_step == 0.0 || y_step == 0.0 {
            tracing::debug!("tiling pattern {name}: degenerate steps");
            return (None, Color::gray(0.5));
        }
        let paint_type = dict.get_i64("PaintType").unwrap_or(1);
        let def = TilingPatternDef {
            content,
            dict: dict.clone(),
            bbox,
            x_step,
            y_step,
            matrix,
            paint_type,
        };
        (
            Some(PatternPaint::Tiling(std::sync::Arc::new(def))),
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
        let Some(PatternPaint::Tiling(def)) = pattern else {
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
        use crate::shading::{MeshTriangle, MeshVertex, ShadingDef, ShadingKind};

        // Mesh shadings are stream objects; get the decoded bytes.
        let resolved = self.resolve_plain(obj);
        let PdfObject::Stream(stream) = &resolved else {
            tracing::debug!("mesh shading type {shading_type} is not a stream");
            return None;
        };
        let data = zpdf_parser::filters::decode_stream(&stream.data, &stream.dict).ok()?;

        let bits_coord = dict.get_i64("BitsPerCoordinate").ok()? as u32;
        let bits_comp = dict.get_i64("BitsPerComponent").ok()? as u32;
        let bits_flag = dict.get_i64("BitsPerFlag").unwrap_or(8) as u32;
        if !(1..=32).contains(&bits_coord) || !(1..=16).contains(&bits_comp) {
            return None;
        }
        let vertices_per_row = dict.get_i64("VerticesPerRow").unwrap_or(0).max(0) as usize;

        // Colour space and optional /Function (all the `&mut self` / `&self`
        // resolution happens before the resolver closure borrows `self`).
        let cs = dict
            .get("ColorSpace")
            .map(|o| self.resolve_color_space_obj(o, 0))
            .unwrap_or(ActiveColorSpace::DeviceRGB);
        let funcs: Vec<zpdf_color::PdfFunction> = match dict.get("Function") {
            Some(f) => match self.resolve_plain(f) {
                PdfObject::Array(arr) => {
                    arr.iter().filter_map(|o| self.parse_function(o)).collect()
                }
                _ => self.parse_function(f).into_iter().collect(),
            },
            None => Vec::new(),
        };
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

        let tris = crate::mesh::decode_mesh(shading_type, &data, &params, &mut resolve);
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

    /// Intersect the running clip-bounds with a new clip path's page-space bbox.
    fn intersect_clip_bounds(&mut self, new: Option<Rect>) {
        let Some(n) = new.map(|r| r.normalize()) else {
            return;
        };
        self.current.clip_bounds = Some(match self.current.clip_bounds {
            Some(c) => {
                let c = c.normalize();
                Rect::new(
                    c.x0.max(n.x0),
                    c.y0.max(n.y0),
                    c.x1.min(n.x1),
                    c.y1.min(n.y1),
                )
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
        let (sh_obj, cache_id) = match self.lookup_res(|r| r.shadings_inline.get(name).cloned()) {
            Some(o) => (o, None),
            None => match self.lookup_res(|r| r.shadings.get(name).copied()) {
                Some(id) => (PdfObject::Ref(id), Some(id)),
                None => {
                    tracing::debug!("shading {name} not found in resources");
                    return;
                }
            },
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

    /// Rasterize `def` over the page-space `region` and emit it as an image.
    fn emit_shading_image(&mut self, def: &crate::shading::ShadingDef, region: Rect) {
        if region.width() <= 0.0 || region.height() <= 0.0 {
            return;
        }
        // Resolution: smooth gradients upscale cleanly; cap the long side.
        let long = region.width().max(region.height());
        let scale = (768.0 / long).min(2.0);
        let w = ((region.width() * scale).ceil() as u32).clamp(1, 2048);
        let h = ((region.height() * scale).ceil() as u32).clamp(1, 2048);
        let Some(buf) = crate::shading::rasterize(def, region, w, h) else {
            return;
        };
        let image = zpdf_image::DecodedImage {
            width: w,
            height: h,
            data: buf,
            has_alpha: true,
            premultiplied: true,
        };
        let image_cache = match self.image_cache.as_mut() {
            Some(c) => c,
            None => return,
        };
        let image_id = image_cache.insert(image);
        // Unit-square image -> page-space region.
        let transform = Matrix::new(
            region.width(),
            0.0,
            0.0,
            region.height(),
            region.x0,
            region.y0,
        );
        let alpha = self.current.fill_alpha;
        self.emit_painted(RenderCommand::DrawImage(ImageDraw {
            image_id,
            transform,
            alpha,
        }));
    }

    /// Page-space bounding box of a path (for clipping shading fills).
    fn path_bounds(path: &Path) -> Option<Rect> {
        let mut min = Point::new(f64::INFINITY, f64::INFINITY);
        let mut max = Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut acc = |p: &Point| {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        };
        for elem in &path.elements {
            match elem {
                PathElement::MoveTo(p) | PathElement::LineTo(p) => acc(p),
                PathElement::CurveTo(c1, c2, p) => {
                    acc(c1);
                    acc(c2);
                    acc(p);
                }
                PathElement::Close => {}
            }
        }
        (min.x.is_finite() && min.y.is_finite() && max.x > min.x && max.y > min.y)
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
            PdfObject::Name(n) => {
                let obj = self
                    .lookup_res(|r| r.properties_inline.get(n.as_str()).cloned())
                    .map(PdfObject::Dict)
                    .or_else(|| {
                        self.lookup_res(|r| r.properties.get(n.as_str()).copied())
                            .map(PdfObject::Ref)
                    });
                match obj {
                    Some(o) => self.oc_object_visible(&o, 0),
                    None => true,
                }
            }
            other => self.oc_object_visible(other, 0),
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
            let Some(ap_id) = a.appearance else { continue };
            let Ok(obj) = file.resolve(ap_id) else {
                continue;
            };
            let PdfObject::Stream(stream) = obj else {
                continue;
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

            let a_mat = Matrix::translate(a.rect.x0, a.rect.y0)
                .concat(&Matrix::scale(a.rect.width() / tw, a.rect.height() / th))
                .concat(&Matrix::translate(-tx0, -ty0));

            let saved_ctm = self.current.ctm;
            self.current.ctm = self.base_ctm.concat(&a_mat);
            self.do_form_xobject(ap_id, &stream, file);
            self.current.ctm = saved_ctm;
        }
    }

    /// Emit a painting command, bracketed in a blend group when a non-Normal
    /// blend mode or a soft mask is active (backends composite the group with
    /// that mode/mask).
    fn emit_painted(&mut self, cmd: RenderCommand) {
        if self.oc_suppressed() {
            return;
        }
        if self.current.blend_mode != BlendMode::Normal || self.current.soft_mask.is_some() {
            self.display_list.push(RenderCommand::PushBlendGroup {
                blend_mode: self.current.blend_mode,
                // A single object carrying a blend mode / soft mask is equivalent
                // to an isolated group of one element composited with that mode —
                // it must NOT pull in the backdrop (which would change a
                // semi-transparent blended fill).
                isolated: true,
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
        // Try inline dict first (common in TikZ/PGF-generated PDFs)
        if let Some(dict) = self.lookup_res(|r| r.ext_g_state_inline.get(name).cloned()) {
            self.apply_ext_gstate_dict(&dict, None);
            return;
        }

        let file = match self.file {
            Some(f) => f,
            None => return,
        };

        let gs_id = match self.lookup_res(|r| r.ext_g_state.get(name).copied()) {
            Some(id) => id,
            None => return,
        };

        let obj = match file.resolve(gs_id) {
            Ok(o) => o,
            Err(_) => return,
        };

        if let Ok(dict) = obj.as_dict() {
            let dict = dict.clone();
            self.apply_ext_gstate_dict(&dict, Some(gs_id));
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
        // /RI: rendering intent name.
        if let Ok(ri) = dict.get_name("RI") {
            self.current.rendering_intent = zpdf_color::RenderIntent::from_pdf_name(ri);
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

    /// Build a soft mask, going through the tiling-pattern reuse cache when
    /// one is installed. Within one tile loop, the same `gs` site recurs once
    /// per tile under CTMs sharing the linear part, and the mask group's
    /// commands come out identical up to a page-space translation, which the
    /// backends apply via [`SoftMask::offset`]. The first build is rebased to
    /// the loop's canonical tile (interior to the page, so the page-rect
    /// raster window covers its whole cell); every use — the building tile
    /// included — then samples the mask only inside its own cell, whose
    /// pre-image under the shift is inside that covered window. Outside tile
    /// loops (`soft_mask_reuse` is None) every mask is built fresh, as before.
    fn build_or_reuse_soft_mask(
        &mut self,
        sm: &zpdf_core::PdfDict,
        gs_id: Option<ObjectId>,
    ) -> Option<SoftMask> {
        let key = match (gs_id, self.soft_mask_reuse.is_some()) {
            (Some(id), true) => Some(SoftMaskReuseKey {
                op_index: self.tile_op_index,
                gs_id: id,
                ctm_linear: [
                    self.current.ctm.a.to_bits(),
                    self.current.ctm.b.to_bits(),
                    self.current.ctm.c.to_bits(),
                    self.current.ctm.d.to_bits(),
                ],
            }),
            _ => None,
        };
        let Some(key) = key else {
            return self.build_soft_mask(sm);
        };

        if !self
            .soft_mask_reuse
            .as_ref()
            .is_some_and(|r| r.masks.contains_key(&key))
        {
            // Miss: build at the canonical tile position. A page-space
            // translation left-multiplies the CTM, i.e. only shifts (e, f),
            // so adding the current→canonical delta is exact whatever `cm`s
            // the cell applied on top of the tile CTM.
            //
            // The reuse context is taken out for the duration of the build:
            // a `gs` inside the mask group itself must build fresh, not
            // insert group-context entries whose op indices could collide
            // with later cell sites (this also freezes the tile op counter,
            // keeping site numbering identical between build- and hit-tiles).
            let reuse = self.soft_mask_reuse.take()?;
            let delta = reuse.cur_delta;
            let saved_ctm_ef = (self.current.ctm.e, self.current.ctm.f);
            let saved_base_ef = (self.base_ctm.e, self.base_ctm.f);
            self.current.ctm.e += delta.0;
            self.current.ctm.f += delta.1;
            self.base_ctm.e += delta.0;
            self.base_ctm.f += delta.1;
            let built = self.build_soft_mask(sm);
            let (build_e, build_f) = (self.current.ctm.e, self.current.ctm.f);
            (self.current.ctm.e, self.current.ctm.f) = saved_ctm_ef;
            (self.base_ctm.e, self.base_ctm.f) = saved_base_ef;
            self.soft_mask_reuse = Some(reuse);
            let mask = built?;
            if let Some(reuse) = self.soft_mask_reuse.as_mut() {
                reuse.masks.insert(
                    key,
                    SoftMaskReuseEntry {
                        mask,
                        build_e,
                        build_f,
                    },
                );
            }
        }

        let entry = self.soft_mask_reuse.as_ref()?.masks.get(&key)?;
        let mut mask = entry.mask.clone();
        // Equal linear parts ⇒ the page-space delta of any point is exactly
        // the CTM translation delta.
        mask.offset = (
            (self.current.ctm.e - entry.build_e) as f32,
            (self.current.ctm.f - entry.build_f) as f32,
        );
        Some(mask)
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
        self.current.soft_mask = None;
        self.current.blend_mode = BlendMode::Normal;
        self.current.fill_alpha = 1.0;
        self.current.stroke_alpha = 1.0;
        self.do_form_xobject(g_ref, &g_stream, file);
        self.current = saved_state;
        self.oc_hidden_from = saved_oc_hidden;
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

    fn execute_do(&mut self, name: &str) {
        let file = match self.file {
            Some(f) => f,
            _ => return,
        };

        let xobj_id = match self.lookup_res(|r| r.xobjects.get(name).copied()) {
            Some(id) => id,
            None => {
                tracing::warn!("XObject not found: {name}");
                return;
            }
        };

        let obj = match file.resolve(xobj_id) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("failed to resolve XObject {name}: {e}");
                return;
            }
        };

        let stream = match obj.as_stream() {
            Ok(s) => s,
            Err(_) => return,
        };

        // Hidden optional content: skip the XObject entirely (both the
        // surrounding BDC /OC state and the XObject's own /OC key).
        if self.oc_suppressed() {
            return;
        }
        if let Some(oc) = stream.dict.get("OC") {
            if !self.oc_object_visible(oc, 0) {
                return;
            }
        }

        let subtype = stream.dict.get_name("Subtype").unwrap_or_default();

        match subtype {
            "Image" => self.do_image_xobject(xobj_id, stream),
            "Form" => self.do_form_xobject(xobj_id, stream, file),
            _ => {
                tracing::warn!("unknown XObject subtype: {subtype}");
            }
        }
    }

    fn do_image_xobject(&mut self, obj_id: zpdf_core::ObjectId, stream: &zpdf_core::PdfStream) {
        // Fast path: this image object was already decoded — re-emit it under the
        // current CTM/alpha without re-decoding (huge win for repeated symbols).
        if let Some(&image_id) = self.image_obj_cache.get(&obj_id) {
            let cmd = RenderCommand::DrawImage(ImageDraw {
                image_id,
                transform: self.current.ctm,
                alpha: self.current.fill_alpha,
            });
            self.emit_painted(cmd);
            return;
        }

        let file = match self.file {
            Some(f) => f,
            None => return,
        };

        let decoded_data = match file.resolve_stream_data(obj_id) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("failed to decode image stream: {e}");
                return;
            }
        };

        // Image metadata keys may be indirect references; resolve them so e.g.
        // an indirect /Decode does not silently invert an /ImageMask stencil.
        let image_dict = resolve_image_metadata(file, &stream.dict);
        // An image's /Intent overrides the graphics-state rendering intent.
        let intent = image_dict
            .get_name("Intent")
            .ok()
            .map(zpdf_color::RenderIntent::from_pdf_name)
            .unwrap_or(self.current.rendering_intent);
        let colorspace = resolve_image_colorspace(
            Some(file),
            self.icc_cache.as_deref_mut(),
            image_dict.get("ColorSpace"),
            0,
            intent,
        );
        let mut image = match zpdf_image::decode_image_xobject_resolved(
            &decoded_data,
            &image_dict,
            self.fill_rgb_u8(),
            colorspace,
        ) {
            Ok(img) => img,
            Err(e) => {
                tracing::warn!("failed to decode image: {e}");
                return;
            }
        };

        // /SMask (soft mask) decodes through the full image path — filters,
        // predictors, any bpc, /Decode — and its gray level becomes the alpha.
        // /Mask as a stream ref is a 1-bpc stencil; /Mask colour-key arrays
        // were already handled inside decode_image_xobject_resolved.
        if let Ok(smask_ref) = stream.dict.get_ref("SMask") {
            fold_soft_mask(&mut image, smask_ref, file);
        } else if let Ok(mask_ref) = stream.dict.get_ref("Mask") {
            fold_stencil_mask(&mut image, mask_ref, file);
        }

        // Insert once and remember the id so repeat draws skip the decode above.
        // `/ImageMask` stencils bake in the fill colour, so they aren't cached.
        let is_stencil = matches!(image_dict.get("ImageMask"), Some(PdfObject::Bool(true)));
        let Some(cache) = self.image_cache.as_mut() else {
            return;
        };
        let image_id = cache.insert(image);
        if !is_stencil {
            self.image_obj_cache.insert(obj_id, image_id);
        }
        let cmd = RenderCommand::DrawImage(ImageDraw {
            image_id,
            transform: self.current.ctm,
            alpha: self.current.fill_alpha,
        });
        self.emit_painted(cmd);
    }

    fn do_inline_image(&mut self, dict: zpdf_core::PdfDict, data: Vec<u8>) {
        if self.image_cache.is_none() || self.oc_suppressed() {
            return;
        }
        let mut norm = normalize_inline_image_dict(&dict);
        let decoded = match zpdf_parser::filters::decode_stream(&data, &norm) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("inline image: filter decode failed: {e}");
                return;
            }
        };
        // An inline-image /CS naming a non-device space refers to a
        // /ColorSpace resource; substitute the resolved object.
        if let Some(PdfObject::Name(n)) = norm.get("ColorSpace") {
            let cs_name = n.0.clone();
            if !matches!(
                cs_name.as_str(),
                "DeviceGray" | "G" | "DeviceRGB" | "RGB" | "DeviceCMYK" | "CMYK"
            ) {
                let from_res = self
                    .lookup_res(|r| r.color_spaces_inline.get(&cs_name).cloned())
                    .or_else(|| {
                        self.lookup_res(|r| r.color_spaces.get(&cs_name).copied())
                            .map(PdfObject::Ref)
                    });
                if let Some(obj) = from_res {
                    norm.0.insert(zpdf_core::PdfName("ColorSpace".into()), obj);
                }
            }
        }
        let intent = norm
            .get_name("Intent")
            .ok()
            .map(zpdf_color::RenderIntent::from_pdf_name)
            .unwrap_or(self.current.rendering_intent);
        let colorspace = resolve_image_colorspace(
            self.file,
            self.icc_cache.as_deref_mut(),
            norm.get("ColorSpace"),
            0,
            intent,
        );
        match zpdf_image::decode_image_xobject_resolved(
            &decoded,
            &norm,
            self.fill_rgb_u8(),
            colorspace,
        ) {
            Ok(img) => self.emit_draw_image(img),
            Err(e) => tracing::warn!("inline image: {e}"),
        }
    }

    /// Current fill colour as 8-bit RGB, for painting `/ImageMask` stencils.
    fn fill_rgb_u8(&self) -> [u8; 3] {
        let c = self.current.fill_color;
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [to_u8(c.r), to_u8(c.g), to_u8(c.b)]
    }

    fn emit_draw_image(&mut self, image: zpdf_image::DecodedImage) {
        let image_cache = match self.image_cache.as_mut() {
            Some(c) => c,
            None => return,
        };
        let image_id = image_cache.insert(image);
        let cmd = RenderCommand::DrawImage(ImageDraw {
            image_id,
            transform: self.current.ctm,
            alpha: self.current.fill_alpha,
        });
        self.emit_painted(cmd);
    }

    fn do_form_xobject(
        &mut self,
        xobj_id: ObjectId,
        stream: &zpdf_core::PdfStream,
        file: &PdfFile,
    ) {
        const MAX_FORM_DEPTH: u32 = 16;
        if self.form_depth >= MAX_FORM_DEPTH {
            tracing::warn!("form XObject nesting exceeds {MAX_FORM_DEPTH}; skipping");
            return;
        }
        // Stop descending once the page budget is spent — bounds exponential
        // form fanout (depth is capped but repetition is not).
        if self.over_budget() {
            return;
        }

        // resolve_stream_data handles decryption, indirect /Filter refs and
        // caching; fall back to direct decode for synthetic streams.
        let decoded = match file
            .resolve_stream_data(xobj_id)
            .or_else(|_| zpdf_parser::filters::decode_stream(&stream.data, &stream.dict))
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("failed to decode form XObject: {e}");
                return;
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
        self.load_form_fonts(&stream.dict, file);

        // The form's full /Resources (xobjects, gstates, colorspaces,
        // patterns, shadings) shadow the page's for the duration of the form.
        let own_resources = match stream.dict.get("Resources") {
            Some(res_obj) => {
                let resolved = self.resolve_dict_of(res_obj);
                resolved.and_then(|d| zpdf_document::page::parse_resource_dict(&d, file).ok())
            }
            None => None,
        };
        let pushed_resources = own_resources.is_some();
        if let Some(res) = own_resources {
            self.form_resources.push(res);
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
        // suppressed) page-level marked-content state.
        let saved_mc_depth = self.mc_depth;
        let saved_oc_hidden = self.oc_hidden_from;

        self.current.ctm = self.current.ctm.concat(&form_matrix);
        // Pattern space inside the form anchors to the form's default space
        // (the CTM established by Do + the form /Matrix), not the page's.
        let saved_base_ctm = self.base_ctm;
        self.base_ctm = self.current.ctm;
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
                    self.display_list.push(RenderCommand::PushClip {
                        path: page_path,
                        rule: FillRule::NonZero,
                    });
                    bbox_clip = true;
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
        let tokenizer = ContentTokenizer::new(&decoded);
        for token in tokenizer {
            if self.over_budget() {
                break;
            }
            match token {
                ContentToken::Operand(obj) => {
                    self.operand_stack.push(obj);
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
        self.form_depth -= 1;

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
        self.oc_hidden_from = saved_oc_hidden;
        self.base_ctm = saved_base_ctm;
        self.form_font_overrides.pop();
        if pushed_resources {
            self.form_resources.pop();
        }
    }

    fn resolve_font_name(&self, name: &str) -> String {
        for overrides in self.form_font_overrides.iter().rev() {
            if let Some(mapped) = overrides.get(name) {
                return mapped.clone();
            }
        }
        name.to_string()
    }

    fn load_form_fonts(&mut self, form_dict: &zpdf_core::PdfDict, file: &PdfFile) {
        let fonts_dict = match form_dict.get("Resources") {
            Some(PdfObject::Dict(res)) => match res.get("Font") {
                Some(PdfObject::Dict(f)) => Some(f.clone()),
                _ => None,
            },
            Some(PdfObject::Ref(r)) => file
                .resolve(*r)
                .ok()
                .and_then(|o| o.as_dict().ok().cloned())
                .and_then(|d| match d.get("Font") {
                    Some(PdfObject::Dict(f)) => Some(f.clone()),
                    _ => None,
                }),
            _ => None,
        };

        let fonts = match fonts_dict {
            Some(f) => f,
            None => {
                self.form_font_overrides.push(HashMap::new());
                return;
            }
        };

        // Build page font ObjectId mapping for collision detection
        let page_font_ids: HashMap<String, ObjectId> = self
            .resources
            .map(|r| r.fonts.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();

        let mut overrides = HashMap::new();
        let form_depth = self.form_font_overrides.len();

        let fc = match self.font_cache.as_mut() {
            Some(fc) => fc,
            None => {
                self.form_font_overrides.push(HashMap::new());
                return;
            }
        };

        for (name, obj) in &fonts.0 {
            if let PdfObject::Ref(font_ref) = obj {
                let page_has_same_name = page_font_ids.contains_key(&name.0);
                let page_has_same_obj = page_font_ids
                    .get(&name.0)
                    .map(|id| *id == *font_ref)
                    .unwrap_or(false);

                if page_has_same_name && page_has_same_obj {
                    continue;
                }

                if page_has_same_name && !page_has_same_obj {
                    let unique_name = format!("__form{}_{}", form_depth, name.0);
                    if fc.get_by_name(&unique_name).is_none() {
                        match zpdf_document::font_loader::load_single_font(file, *font_ref) {
                            Ok(font) => {
                                fc.insert(unique_name.clone(), font);
                            }
                            Err(e) => {
                                tracing::debug!("form font {}: {e}", name.0);
                                fc.insert(
                                    unique_name.clone(),
                                    zpdf_font::LoadedFont::new_placeholder(name.0.clone()),
                                );
                            }
                        }
                    }
                    overrides.insert(name.0.clone(), unique_name);
                } else if fc.get_by_name(&name.0).is_none() {
                    match zpdf_document::font_loader::load_single_font(file, *font_ref) {
                        Ok(font) => {
                            fc.insert(name.0.clone(), font);
                        }
                        Err(e) => {
                            tracing::debug!("form font {}: {e}", name.0);
                            fc.insert(
                                name.0.clone(),
                                zpdf_font::LoadedFont::new_placeholder(name.0.clone()),
                            );
                        }
                    }
                }
            }
        }

        self.form_font_overrides.push(overrides);
    }

    fn transform_path_to_page_space(&self, path: &Path) -> Path {
        let ctm = &self.current.ctm;
        if *ctm == Matrix::identity() {
            return path.clone();
        }
        let mut result = Path::new();
        for elem in &path.elements {
            match *elem {
                PathElement::MoveTo(p) => {
                    result.move_to(p.transform(ctm));
                }
                PathElement::LineTo(p) => {
                    result.line_to(p.transform(ctm));
                }
                PathElement::CurveTo(c1, c2, end) => {
                    result.curve_to(c1.transform(ctm), c2.transform(ctm), end.transform(ctm));
                }
                PathElement::Close => {
                    result.close();
                }
            }
        }
        result
    }

    fn current_point(&self) -> Point {
        for elem in self.current_path.elements.iter().rev() {
            match *elem {
                PathElement::MoveTo(p) | PathElement::LineTo(p) | PathElement::CurveTo(_, _, p) => {
                    return p
                }
                PathElement::Close => {}
            }
        }
        Point::zero()
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

    fn paint_stroke(&mut self) {
        let path = std::mem::take(&mut self.current_path);
        if path.is_empty() {
            return;
        }
        let page_path = self.transform_path_to_page_space(&path);
        self.emit_stroke(page_path);
    }

    fn paint_fill(&mut self, rule: FillRule) {
        let path = std::mem::take(&mut self.current_path);
        if path.is_empty() {
            return;
        }
        let page_path = self.transform_path_to_page_space(&path);
        self.fill_page_path(page_path, rule);
    }

    fn paint_fill_then_stroke(&mut self, rule: FillRule) {
        let path = std::mem::take(&mut self.current_path);
        if path.is_empty() {
            return;
        }
        let page_path = self.transform_path_to_page_space(&path);
        self.fill_page_path(page_path.clone(), rule);
        self.emit_stroke(page_path);
    }

    /// Stroke a page-space path. A stroke pattern (tiling or shading) is clipped
    /// to the stroke outline via `PushClipStroke`; everything else is a solid
    /// stroke (shading patterns fall back to the precomputed average colour when
    /// the region is degenerate).
    fn emit_stroke(&mut self, page_path: Path) {
        let style = self.stroke_style();
        if let Some(pat) = self.current.stroke_pattern.clone() {
            // The stroke region is the centerline extent grown by the stroke
            // width. Unlike `path_bounds`, a flat path (a horizontal/vertical
            // line) is fine here — growing makes the region two-dimensional.
            if let Some(bounds) = Self::stroke_region_bounds(&page_path, self.current.line_width) {
                self.display_list.push(RenderCommand::PushClipStroke {
                    path: page_path.clone(),
                    style: style.clone(),
                });
                let painted = self.paint_pattern_in_region(&pat, bounds);
                self.display_list.push(RenderCommand::PopClip);
                if painted {
                    return;
                }
                // Tiling failed to replicate: fall through to a solid stroke
                // (the clip pair above is balanced and painted nothing).
            }
        }
        let cmd = RenderCommand::StrokePath {
            path: page_path,
            style,
            paint: Paint::Solid(self.current.stroke_color),
            alpha: self.current.stroke_alpha,
        };
        self.emit_painted(cmd);
    }

    /// Page-space bounding box of a stroke: the path's point extent grown by
    /// the stroke width on every side (so even a degenerate flat path yields a
    /// 2-D region for the pattern paint to cover).
    fn stroke_region_bounds(path: &Path, line_width: f32) -> Option<Rect> {
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
        for elem in &path.elements {
            match elem {
                PathElement::MoveTo(p) | PathElement::LineTo(p) => acc(p),
                PathElement::CurveTo(c1, c2, p) => {
                    acc(c1);
                    acc(c2);
                    acc(p);
                }
                PathElement::Close => {}
            }
        }
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
            return None;
        }
        let grow = (line_width.max(1.0) as f64).max(0.5);
        Some(Rect::new(x0 - grow, y0 - grow, x1 + grow, y1 + grow))
    }

    /// Fill an already page-space path with the active fill paint — a pattern
    /// (tiling or shading) is clipped to the path; everything else is a solid
    /// fill.
    fn fill_page_path(&mut self, page_path: Path, rule: FillRule) {
        if let Some(pat) = self.current.fill_pattern.clone() {
            if let Some(bounds) = Self::path_bounds(&page_path) {
                self.display_list.push(RenderCommand::PushClip {
                    path: page_path.clone(),
                    rule,
                });
                let painted = self.paint_pattern_in_region(&pat, bounds);
                self.display_list.push(RenderCommand::PopClip);
                if painted {
                    return;
                }
                // Fall through: solid approximation (the clip pair above is
                // balanced and harmless).
            }
        }
        let cmd = RenderCommand::FillPath {
            path: page_path,
            rule,
            paint: Paint::Solid(self.current.fill_color),
            alpha: self.current.fill_alpha,
        };
        self.emit_painted(cmd);
    }

    /// Paint a pattern (tiling or shading) across `bounds`. The caller must
    /// already have installed the clip region (a path/stroke/glyph clip).
    /// Returns false when a tiling pattern could not be replicated, so the
    /// caller can fall back to a solid approximation.
    fn paint_pattern_in_region(&mut self, pattern: &PatternPaint, bounds: Rect) -> bool {
        match pattern {
            PatternPaint::Shading(def) => {
                self.emit_shading_image(def, bounds);
                true
            }
            PatternPaint::Tiling(def) => self.paint_tiling_pattern(def, bounds),
        }
    }

    /// Replicate a tiling-pattern cell across the page-space `bounds` of a
    /// fill. Returns false when the pattern is degenerate or the tile count
    /// explodes, in which case the caller falls back to a solid fill.
    fn paint_tiling_pattern(&mut self, def: &TilingPatternDef, bounds: Rect) -> bool {
        const MAX_FORM_DEPTH: u32 = 16;
        const MAX_TILES: i64 = 4096;
        if self.oc_suppressed() {
            // Hidden layer: skip the tile replication work entirely (the
            // surrounding clip pair is balanced and paints nothing).
            return true;
        }
        if self.form_depth >= MAX_FORM_DEPTH {
            tracing::warn!("tiling pattern nesting exceeds {MAX_FORM_DEPTH}; skipping");
            return false;
        }
        // Pattern space is anchored to the page's default user space, not the
        // CTM at fill time.
        let to_page = self.base_ctm.concat(&def.matrix);
        let Some(from_page) = to_page.inverse() else {
            return false;
        };

        // Pattern-space bbox of the fill region: map all four page-space
        // corners (the matrix may rotate/skew).
        let corners = [
            Point::new(bounds.x0, bounds.y0),
            Point::new(bounds.x1, bounds.y0),
            Point::new(bounds.x0, bounds.y1),
            Point::new(bounds.x1, bounds.y1),
        ];
        let (mut px0, mut py0) = (f64::INFINITY, f64::INFINITY);
        let (mut px1, mut py1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for c in &corners {
            let p = c.transform(&from_page);
            px0 = px0.min(p.x);
            py0 = py0.min(p.y);
            px1 = px1.max(p.x);
            py1 = py1.max(p.y);
        }
        if !(px0.is_finite() && py0.is_finite() && px1.is_finite() && py1.is_finite()) {
            return false;
        }

        // Tile (i, j) covers bbox translated by (i·XStep, j·YStep). Conservative
        // index ranges from the two extreme offsets (steps may be negative).
        let range = |lo: f64, hi: f64, cell_lo: f64, cell_hi: f64, step: f64| {
            let t0 = (lo - cell_hi) / step;
            let t1 = (hi - cell_lo) / step;
            let (a, b) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
            (a.floor() as i64, b.ceil() as i64)
        };
        let (i0, i1) = range(px0, px1, def.bbox.x0, def.bbox.x1, def.x_step);
        let (j0, j1) = range(py0, py1, def.bbox.y0, def.bbox.y1, def.y_step);
        // Saturating: pathologically small steps push the f64→i64 casts to
        // the integer extremes, and the raw subtraction would overflow.
        let nx = i1.saturating_sub(i0).saturating_add(1);
        let ny = j1.saturating_sub(j0).saturating_add(1);
        if nx <= 0 || ny <= 0 {
            return false;
        }
        if nx.saturating_mul(ny) > MAX_TILES {
            tracing::warn!(
                "tiling pattern: {nx}x{ny} tiles exceeds {MAX_TILES}; using solid approximation"
            );
            return false;
        }

        let file = match self.file {
            Some(f) => f,
            None => return false,
        };

        // Pattern /Resources and fonts shadow the page's for the cell body,
        // exactly like a form XObject's.
        self.load_form_fonts(&def.dict, file);
        let own_resources = def
            .dict
            .get("Resources")
            .and_then(|res_obj| self.resolve_dict_of(res_obj))
            .and_then(|d| zpdf_document::page::parse_resource_dict(&d, file).ok());
        let pushed_resources = own_resources.is_some();
        if let Some(res) = own_resources {
            self.form_resources.push(res);
        }

        let saved_text_matrix = self.text_matrix;
        let saved_text_line_matrix = self.text_line_matrix;
        let saved_operand_stack = std::mem::take(&mut self.operand_stack);
        let saved_suppress = self.suppress_color_ops;
        // Unbalanced BDC/EMC inside a cell must not leak marked-content
        // suppression into later tiles or the rest of the page.
        let saved_mc_depth = self.mc_depth;
        let saved_oc_hidden = self.oc_hidden_from;
        // Patterns selected inside the cell anchor to the cell's space.
        let saved_base_ctm = self.base_ctm;
        let uncolored = def.paint_type == 2;
        // The scn-time color paints an uncolored cell.
        let pattern_color = self.current.fill_color;
        let cell_alpha = self.current.fill_alpha;
        // Soft-mask reuse across tiles (see build_or_reuse_soft_mask): masks
        // are built as if at the middle tile of the range, whose cell sits in
        // the page interior. Saved and restored so nested pattern loops don't
        // share each other's keys. (Overflow-safe midpoint: i0/i1 can sit at
        // the i64 extremes for pathological steps.)
        let canonical_ctm = to_page.concat(&Matrix::translate(
            (i0 + (i1 - i0) / 2) as f64 * def.x_step,
            (j0 + (j1 - j0) / 2) as f64 * def.y_step,
        ));
        let saved_mask_reuse = self.soft_mask_reuse.replace(TileMaskReuse {
            masks: HashMap::new(),
            cur_delta: (0.0, 0.0),
        });
        let saved_tile_op_index = self.tile_op_index;

        self.form_depth += 1;
        'tiles: for j in j0..=j1 {
            for i in i0..=i1 {
                // A pathological /XStep,/YStep vs clip can imply millions of
                // tiles; stop replicating once the page hits the command budget.
                if self.over_budget() {
                    tracing::warn!("tiling pattern exceeded command budget; truncating");
                    break 'tiles;
                }
                let tile_offset = Matrix::translate(i as f64 * def.x_step, j as f64 * def.y_step);
                let tile_ctm = to_page.concat(&tile_offset);
                // Restart the per-tile operator count so `gs` sites line up
                // across tiles, and point mask builds at the canonical tile.
                self.tile_op_index = 0;
                if let Some(reuse) = self.soft_mask_reuse.as_mut() {
                    reuse.cur_delta = (canonical_ctm.e - tile_ctm.e, canonical_ctm.f - tile_ctm.f);
                }

                // Per-tile state: cells start from a clean color state (and
                // must not re-enter the active pattern), inherit alpha.
                let depth_floor = self.state_stack.len();
                self.state_stack.push(self.current.clone());
                let saved_floor = self.state_floor;
                self.state_floor = depth_floor + 1;
                self.current.ctm = tile_ctm;
                self.current.clip_depth = 0;
                self.current.fill_pattern = None;
                self.current.stroke_pattern = None;
                self.current.fill_cs = ActiveColorSpace::DeviceGray;
                self.current.stroke_cs = ActiveColorSpace::DeviceGray;
                self.current.fill_alpha = cell_alpha;
                self.current.stroke_alpha = cell_alpha;
                if uncolored {
                    self.current.fill_color = pattern_color;
                    self.current.stroke_color = pattern_color;
                    self.suppress_color_ops = true;
                } else {
                    self.current.fill_color = Color::black();
                    self.current.stroke_color = Color::black();
                }
                self.text_matrix = Matrix::identity();
                self.text_line_matrix = Matrix::identity();
                self.base_ctm = tile_ctm;

                // Clip the cell to its /BBox.
                let mut bbox_path = Path::new();
                bbox_path.rect(def.bbox);
                let page_bbox = self.transform_path_to_page_space(&bbox_path);
                self.display_list.push(RenderCommand::PushClip {
                    path: page_bbox,
                    rule: FillRule::NonZero,
                });

                let tokenizer = ContentTokenizer::new(&def.content);
                for token in tokenizer {
                    match token {
                        ContentToken::Operand(obj) => self.operand_stack.push(obj),
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
                self.display_list.push(RenderCommand::PopClip);
                self.state_floor = saved_floor;
                self.suppress_color_ops = saved_suppress;
                self.mc_depth = saved_mc_depth;
                self.oc_hidden_from = saved_oc_hidden;
                self.base_ctm = saved_base_ctm;
            }
        }
        self.form_depth -= 1;
        self.soft_mask_reuse = saved_mask_reuse;
        self.tile_op_index = saved_tile_op_index;

        self.text_matrix = saved_text_matrix;
        self.text_line_matrix = saved_text_line_matrix;
        self.operand_stack = saved_operand_stack;
        self.form_font_overrides.pop();
        if pushed_resources {
            self.form_resources.pop();
        }
        true
    }

    /// Emit a solid-colour glyph run (the non-pattern path, and the fallback
    /// when a tiling text pattern cannot be replicated).
    fn emit_solid_glyph_run(
        &mut self,
        font_id: FontId,
        font_size: f32,
        glyphs: Vec<PositionedGlyph>,
        transform: Matrix,
        h_scale: f32,
    ) {
        let paint_color = match self.current.render_mode {
            1 | 5 => self.current.stroke_color,
            _ => self.current.fill_color,
        };
        let cmd = RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size,
            glyphs,
            paint: Paint::Solid(paint_color),
            alpha: self.current.fill_alpha,
            transform,
            h_scale,
        });
        self.emit_painted(cmd);
    }

    fn show_text(&mut self, bytes: &[u8]) {
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
        let want_text = self.text_sink.is_some();

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

        if is_two_byte {
            // Composite font: segment codes through the /Encoding CMap
            // (Identity-H semantics when absent) and map code → CID → glyph.
            let cmap = font_and_id.and_then(|(_, f)| f.cid_cmap.as_ref());
            vertical = cmap.map(|c| c.wmode == 1).unwrap_or(false);
            let codes_are_unicode = cmap.map(|c| c.codes_are_unicode).unwrap_or(false);
            // Legacy GB EUC CMap on a *substituted* (non-embedded) font: decode
            // code → Unicode and resolve through the system face. Embedded GBpc
            // fonts keep the normal code → CID → GID path.
            let gb_decode = cmap.map(|c| c.gb_decode).unwrap_or(false)
                && font_and_id.map(|(_, f)| f.is_substitute).unwrap_or(false);
            let dw2 = font_and_id.map(|(_, f)| f.dw2).unwrap_or((880.0, -1000.0));

            let mut i = 0usize;
            while i < bytes.len() {
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

                // Resolve the glyph id, its 1/1000-unit advance, and (for CID
                // fonts in vertical mode) the per-CID /W2 vertical metric.
                let (glyph_id, adv_units, v_metric) = if let Some((_, font)) = font_and_id {
                    if codes_are_unicode {
                        match font.unicode_glyph(code) {
                            Some((gid, adv)) => (gid, adv, None),
                            None => (0, 500.0, None),
                        }
                    } else if gb_decode {
                        // GID from the system face via code → Unicode; advance
                        // from the PDF /W keyed by the Adobe-GB1 CID (1-byte
                        // ASCII → CID 1-95; 2-byte CJK → CID 0 → DW, ~1000).
                        let cid = cmap
                            .map(|c| c.code_to_cid(code, len as u8))
                            .unwrap_or(0)
                            .min(u16::MAX as u32) as u16;
                        let gid = cmap
                            .and_then(|c| c.decode_to_unicode(code, len as u8))
                            .and_then(|u| font.unicode_glyph(u))
                            .map(|(g, _)| g)
                            .unwrap_or(0);
                        (gid, font.glyph_advance(cid), None)
                    } else {
                        let cid = cmap
                            .map(|c| c.code_to_cid(code, len as u8))
                            .unwrap_or(code)
                            .min(u16::MAX as u32) as u16;
                        let v_metric = if vertical {
                            font.cid_v_metric(cid)
                        } else {
                            None
                        };
                        (cid, font.glyph_advance(cid), v_metric)
                    }
                } else {
                    (code.min(u16::MAX as u32) as u16, 500.0, None)
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
                    glyphs.push(PositionedGlyph {
                        glyph_id,
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
            for &byte in bytes {
                let code = byte as u16;
                // Map the character code through /Encoding + the font's cmap/charset
                // to the real glyph ID. Without font data, fall back to the raw code.
                let (glyph_id, advance) = if let Some((_, font)) = font_and_id {
                    let gid = font.code_to_gid(code).unwrap_or_else(|| {
                        tracing::debug!(
                            "font {}: code {code} unmapped, using as raw GID",
                            font.base_font
                        );
                        code
                    });
                    let adv = font.simple_glyph_advance(code, gid) as f32 * scale_factor * h_scale;
                    (gid, adv)
                } else {
                    (code, font_size * 0.6 * h_scale)
                };
                glyphs.push(PositionedGlyph {
                    glyph_id,
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

        // Decode the text and compute its user-space placement for extraction,
        // while the immutable font borrow is still live.
        let text_span = if want_text {
            let decoded = font_and_id
                .map(|(_, f)| f.decode_to_string(bytes))
                .unwrap_or_else(|| {
                    bytes
                        .iter()
                        .filter(|&&b| b >= 0x20 && b != 0x7f)
                        .map(|&b| b as char)
                        .collect()
                });
            // The rise is already baked into `combined`.
            let start = Point::new(0.0, 0.0).transform(&combined);
            let end = Point::new(x_offset as f64, 0.0).transform(&combined);
            let dx = end.x - start.x;
            let cscale = ((combined.a * combined.a + combined.b * combined.b).sqrt()
                + (combined.c * combined.c + combined.d * combined.d).sqrt())
                / 2.0;
            Some(TextSpan {
                text: decoded,
                x: start.x,
                y: start.y,
                size: (font_size as f64 * cscale) as f32,
                // Signed horizontal extent of the run (end.x − start.x) so the
                // extraction gap heuristic stays correct under scaling/rotation.
                advance: dx,
            })
        } else {
            None
        };

        // Only emit a glyph run when a font is actually active; with no font the
        // glyph IDs would be raw, unmappable codes aliased onto font 0.
        // Text render mode 3 (invisible, the OCR-layer case) and 7 (clip only)
        // paint nothing; stroke modes 1/2/5/6 are approximated as fills.
        let invisible = matches!(self.current.render_mode, 3 | 7);
        if let Some(font_id) = self.current_font_id {
            if !glyphs.is_empty() && !invisible {
                // A Pattern colour space on the active paint (fill for modes
                // 0/2/4/6, stroke for 1/5) clips the pattern to the glyph
                // outlines instead of painting a solid run.
                let text_pattern = match self.current.render_mode {
                    1 | 5 => self.current.stroke_pattern.clone(),
                    _ => self.current.fill_pattern.clone(),
                };
                let glyph_clip = text_pattern.as_ref().and_then(|_| {
                    font_and_id.and_then(|(_, font)| {
                        build_glyph_clip_path(&glyphs, &combined, font, font_size, h_scale)
                    })
                });
                match (text_pattern, glyph_clip) {
                    (Some(pat), Some((clip_path, bounds))) => {
                        self.display_list.push(RenderCommand::PushClip {
                            path: clip_path,
                            rule: FillRule::NonZero,
                        });
                        let painted = self.paint_pattern_in_region(&pat, bounds);
                        self.display_list.push(RenderCommand::PopClip);
                        if !painted {
                            self.emit_solid_glyph_run(
                                font_id, font_size, glyphs, combined, h_scale,
                            );
                        }
                    }
                    _ => self.emit_solid_glyph_run(font_id, font_size, glyphs, combined, h_scale),
                }
            }
        }

        // Hidden optional-content layers also extract no text (emit_painted
        // already drops the glyph run; the matrix advance below still runs).
        if !self.oc_suppressed() {
            if let (Some(span), Some(sink)) = (text_span, self.text_sink.as_mut()) {
                if !span.text.is_empty() {
                    sink.push(span);
                }
            }
        }

        // Advance the text matrix along the writing direction.
        let advance = if vertical {
            Matrix::translate(0.0, x_offset as f64)
        } else {
            Matrix::translate(x_offset as f64, 0.0)
        };
        self.text_matrix = self.text_matrix.concat(&advance);
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

/// Build a combined glyph-outline clip path (in PAGE space) for a positioned
/// glyph run, plus its bounding box. Used to clip a pattern/shading paint to
/// text. Returns `None` for fonts with no usable outlines (bitmap/Type3) or an
/// all-blank run. Quadratic segments are promoted to cubics for the display
/// list `Path` (which carries cubics only).
fn build_glyph_clip_path(
    glyphs: &[PositionedGlyph],
    tm: &Matrix,
    font: &zpdf_font::LoadedFont,
    font_size: f32,
    h_scale: f32,
) -> Option<(Path, Rect)> {
    if !font.has_font_data() || font.is_type3() {
        return None;
    }
    let upem = font.units_per_em as f32;
    if upem <= 0.0 {
        return None;
    }

    // Glyph-space point (font units, + the glyph's text-space offset) → page.
    // The shape x carries the horizontal-scaling factor (Th); the offset already
    // does (it's an accumulated advance), so Th multiplies only the outline term.
    let to_page = |gx: f64, gy: f64, ox: f32, oy: f32| -> Point {
        let tx = (gx as f32 / upem * font_size * h_scale + ox) as f64;
        let ty = (gy as f32 / upem * font_size + oy) as f64;
        Point::new(tm.a * tx + tm.c * ty + tm.e, tm.b * tx + tm.d * ty + tm.f)
    };

    let mut out = Path::new();
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
            None => continue,
        };
        let (ox, oy) = (glyph.x, glyph.y);
        let mut cur = Point::new(0.0, 0.0);
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
                    track(e, &mut bb);
                    cur = e;
                }
                zpdf_font::OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                    let c1 = to_page(x1, y1, ox, oy);
                    let c2 = to_page(x2, y2, ox, oy);
                    let e = to_page(x, y, ox, oy);
                    out.curve_to(c1, c2, e);
                    track(e, &mut bb);
                    cur = e;
                }
                zpdf_font::OutlineCommand::Close => out.close(),
            }
        }
    }

    if !(any && bb.0.is_finite() && bb.2 > bb.0 && bb.3 > bb.1) {
        return None;
    }
    Some((out, Rect::new(bb.0, bb.1, bb.2, bb.3)))
}

/// Build an ICC transform from an inline (synthetic) profile stream,
/// bypassing the cache — inline streams have no object id to key on.
fn build_inline_icc_transform(
    s: &zpdf_core::PdfStream,
    intent: zpdf_color::RenderIntent,
) -> Option<std::sync::Arc<zpdf_color::IccTransform>> {
    let data = zpdf_parser::filters::decode_stream(&s.data, &s.dict).ok()?;
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
/// 1/3/4 → gray/RGB/CMYK) and Indexed palettes (string or stream; an ICC base
/// is baked into the palette). Returns `None` for spaces it cannot resolve,
/// letting zpdf-image fall back to its own inference.
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
                            (None, _) => build_inline_icc_transform(stream, intent),
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
                            zpdf_parser::filters::decode_stream(&s.data, &s.dict).ok()?
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
                other => zpdf_image::colorspace_from_name(other),
            }
        }
        _ => None,
    }
}

/// Decode an image's `/SMask` through the full image-decode path (filters,
/// predictors, any bpc, /Decode) and fold it into `image` as premultiplied
/// alpha. Failures leave the image opaque rather than dropping it.
fn fold_soft_mask(
    image: &mut zpdf_image::DecodedImage,
    smask_ref: zpdf_core::ObjectId,
    file: &PdfFile,
) {
    let obj = match file.resolve(smask_ref) {
        Ok(o) => o,
        Err(_) => return,
    };
    let stream = match obj.as_stream() {
        Ok(s) => s,
        Err(_) => return,
    };
    let data = match file.resolve_stream_data(smask_ref) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to decode /SMask stream: {e}");
            return;
        }
    };
    let dict = resolve_image_metadata(file, &stream.dict);
    // SMasks are DeviceGray by definition (spec table 144).
    match zpdf_image::decode_image_xobject_resolved(
        &data,
        &dict,
        [0, 0, 0],
        Some(zpdf_image::ResolvedColorSpace::Gray),
    ) {
        Ok(mask) => zpdf_image::apply_smask_image(image, &mask),
        Err(e) => tracing::warn!("failed to decode /SMask image: {e}"),
    }
}

/// Decode a `/Mask` stencil stream (1 bpc; sample 1 = masked out, `/Decode
/// [1 0]` inverts) and fold it into `image` as premultiplied alpha.
fn fold_stencil_mask(
    image: &mut zpdf_image::DecodedImage,
    mask_ref: zpdf_core::ObjectId,
    file: &PdfFile,
) {
    let obj = match file.resolve(mask_ref) {
        Ok(o) => o,
        Err(_) => return,
    };
    let stream = match obj.as_stream() {
        Ok(s) => s,
        // A /Mask ref can also point at a colour-key array, which
        // resolve_image_metadata already inlined for the image decoder.
        Err(_) => return,
    };
    let data = match file.resolve_stream_data(mask_ref) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to decode /Mask stencil stream: {e}");
            return;
        }
    };
    let dict = resolve_image_metadata(file, &stream.dict);
    let width = dict.get_i64("Width").unwrap_or(0) as u32;
    let height = dict.get_i64("Height").unwrap_or(0) as u32;
    let invert = zpdf_image::mask_decode_inverts(&dict);
    zpdf_image::apply_stencil_mask(image, &data, width, height, invert);
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

    #[test]
    fn interpret_save_restore() {
        let content = b"q 0.5 g 100 100 50 50 re f Q 100 100 50 50 re f";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let dl = ContentInterpreter::new(page_rect).interpret(content);
        assert_eq!(dl.commands.len(), 2);
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
                x: 0.0,
                y: 0.0,
                advance: 0.0,
            },
            PositionedGlyph {
                glyph_id: 42,
                x: 50.0,
                y: 0.0,
                advance: 0.0,
            },
        ];
        let (path, bounds) = build_glyph_clip_path(&glyphs, &Matrix::identity(), &font, 100.0, 1.0)
            .expect("glyph clip path");
        assert!(
            !path.elements.is_empty(),
            "clip path should carry outline elements"
        );
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
                    assert!(c.r < 0.01 && c.g < 0.01 && c.b < 0.01);
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
        assert_eq!(
            resolve_image_colorspace(
                None,
                None,
                Some(&cs),
                0,
                zpdf_color::RenderIntent::default()
            ),
            Some(Rcs::Gray)
        );
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
}
