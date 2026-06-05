use zpdf_core::{Matrix, Point, Rect};

#[derive(Debug, Clone)]
pub struct DisplayList {
    pub page_rect: Rect,
    pub commands: Vec<RenderCommand>,
}

impl DisplayList {
    pub fn new(page_rect: Rect) -> Self {
        Self {
            page_rect,
            commands: Vec::new(),
        }
    }

    pub fn push(&mut self, cmd: RenderCommand) {
        self.commands.push(cmd);
    }
}

#[derive(Debug, Clone)]
pub enum RenderCommand {
    FillPath {
        path: Path,
        rule: FillRule,
        paint: Paint,
        alpha: f32,
    },
    StrokePath {
        path: Path,
        style: StrokeStyle,
        paint: Paint,
        alpha: f32,
    },
    DrawGlyphRun(GlyphRun),
    DrawImage(ImageDraw),
    PushClip {
        path: Path,
        rule: FillRule,
    },
    PopClip,
    PushBlendGroup {
        blend_mode: BlendMode,
        isolated: bool,
        knockout: bool,
        bounds: Rect,
    },
    PopBlendGroup,
}

// -- Path --

#[derive(Debug, Clone)]
pub struct Path {
    pub elements: Vec<PathElement>,
}

impl Path {
    pub fn new() -> Self {
        Self {
            elements: Vec::new(),
        }
    }

    pub fn move_to(&mut self, p: Point) {
        self.elements.push(PathElement::MoveTo(p));
    }

    pub fn line_to(&mut self, p: Point) {
        self.elements.push(PathElement::LineTo(p));
    }

    pub fn curve_to(&mut self, c1: Point, c2: Point, end: Point) {
        self.elements.push(PathElement::CurveTo(c1, c2, end));
    }

    pub fn close(&mut self) {
        self.elements.push(PathElement::Close);
    }

    pub fn rect(&mut self, r: Rect) {
        self.move_to(Point::new(r.x0, r.y0));
        self.line_to(Point::new(r.x1, r.y0));
        self.line_to(Point::new(r.x1, r.y1));
        self.line_to(Point::new(r.x0, r.y1));
        self.close();
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }
}

impl Default for Path {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum PathElement {
    MoveTo(Point),
    LineTo(Point),
    CurveTo(Point, Point, Point),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

// -- Stroke --

#[derive(Debug, Clone)]
pub struct StrokeStyle {
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f32,
    pub dash: Option<DashPattern>,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        Self {
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}

#[derive(Debug, Clone)]
pub struct DashPattern {
    pub array: Vec<f32>,
    pub phase: f32,
}

// -- Paint --

#[derive(Debug, Clone, Copy)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn gray(v: f32) -> Self {
        Self::rgb(v, v, v)
    }

    pub fn black() -> Self {
        Self::gray(0.0)
    }

    pub fn white() -> Self {
        Self::gray(1.0)
    }
}

#[derive(Debug, Clone)]
pub enum Paint {
    Solid(Color),
    Pattern(u32),
    Shading(u32),
}

// -- Text --

pub type FontId = u32;
pub type ImageId = u32;

#[derive(Debug, Clone)]
pub struct GlyphRun {
    pub font_id: FontId,
    pub font_size: f32,
    pub glyphs: Vec<PositionedGlyph>,
    pub paint: Paint,
    pub alpha: f32,
    pub transform: Matrix,
}

#[derive(Debug, Clone, Copy)]
pub struct PositionedGlyph {
    pub glyph_id: u16,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
}

// -- Image --

#[derive(Debug, Clone)]
pub struct ImageDraw {
    pub image_id: ImageId,
    pub transform: Matrix,
    pub alpha: f32,
}

// -- Blend --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}
