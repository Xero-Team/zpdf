use zpdf_core::{Matrix, PdfObject, Point, Rect};
use zpdf_display_list::*;
use zpdf_font::FontCache;

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
    font_cache: Option<&'a FontCache>,
    current_font_id: Option<zpdf_font::FontId>,
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
        }
    }

    pub fn with_fonts(mut self, cache: &'a FontCache) -> Self {
        self.font_cache = Some(cache);
        self
    }

    pub fn interpret(mut self, content: &[u8]) -> DisplayList {
        let tokenizer = ContentTokenizer::new(content);

        for token in tokenizer {
            match token {
                ContentToken::Operand(obj) => {
                    self.operand_stack.push(obj);
                }
                ContentToken::Operator(op) => {
                    self.execute_operator(&op);
                    self.operand_stack.clear();
                }
            }
        }

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
        match op {
            // -- Graphics state --
            "q" => self.state_stack.push(self.current.clone()),
            "Q" => {
                if let Some(state) = self.state_stack.pop() {
                    self.current = state;
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
            "i" | "ri" | "gs" => {
                // flatness, rendering intent, ExtGState - consume operands
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
                let path = self.current_path.clone();
                self.display_list.push(RenderCommand::PushClip {
                    path,
                    rule: FillRule::NonZero,
                });
            }
            "W*" => {
                let path = self.current_path.clone();
                self.display_list.push(RenderCommand::PushClip {
                    path,
                    rule: FillRule::EvenOdd,
                });
            }

            // -- Color --
            "g" => {
                let gray = self.pop_f64() as f32;
                self.current.fill_color = Color::gray(gray);
            }
            "G" => {
                let gray = self.pop_f64() as f32;
                self.current.stroke_color = Color::gray(gray);
            }
            "rg" => {
                let b = self.pop_f64() as f32;
                let g = self.pop_f64() as f32;
                let r = self.pop_f64() as f32;
                self.current.fill_color = Color::rgb(r, g, b);
            }
            "RG" => {
                let b = self.pop_f64() as f32;
                let g = self.pop_f64() as f32;
                let r = self.pop_f64() as f32;
                self.current.stroke_color = Color::rgb(r, g, b);
            }
            "k" => {
                let k_val = self.pop_f64() as f32;
                let y_val = self.pop_f64() as f32;
                let m_val = self.pop_f64() as f32;
                let c_val = self.pop_f64() as f32;
                let r = (1.0 - c_val) * (1.0 - k_val);
                let g = (1.0 - m_val) * (1.0 - k_val);
                let b = (1.0 - y_val) * (1.0 - k_val);
                self.current.fill_color = Color::rgb(r, g, b);
            }
            "K" => {
                let k_val = self.pop_f64() as f32;
                let y_val = self.pop_f64() as f32;
                let m_val = self.pop_f64() as f32;
                let c_val = self.pop_f64() as f32;
                let r = (1.0 - c_val) * (1.0 - k_val);
                let g = (1.0 - m_val) * (1.0 - k_val);
                let b = (1.0 - y_val) * (1.0 - k_val);
                self.current.stroke_color = Color::rgb(r, g, b);
            }
            "cs" | "CS" | "sc" | "SC" | "scn" | "SCN" => {
                // Named/complex color spaces - consume operands, use black fallback
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
                if let Some(fc) = self.font_cache {
                    if let Some((fid, _font)) = fc.get_by_name(&name) {
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
                // Consume XObject name
                let _name = self.pop_name();
                // TODO: resolve XObject and render
            }

            // -- Type3 glyph operators --
            "d0" => {
                // wx wy d0: set glyph width (2 operands, consume them)
            }
            "d1" => {
                // wx wy llx lly urx ury d1: set glyph width and bbox (6 operands)
            }

            // -- Marked content --
            "BMC" | "BDC" | "EMC" | "MP" | "DP" => {}

            _ => {}
        }
    }

    fn current_point(&self) -> Point {
        for elem in self.current_path.elements.iter().rev() {
            match *elem {
                PathElement::MoveTo(p)
                | PathElement::LineTo(p)
                | PathElement::CurveTo(_, _, p) => return p,
                PathElement::Close => {}
            }
        }
        Point::zero()
    }

    fn paint_stroke(&mut self) {
        let path = std::mem::take(&mut self.current_path);
        if path.is_empty() {
            return;
        }
        self.display_list.push(RenderCommand::StrokePath {
            path,
            style: StrokeStyle {
                width: self.current.line_width,
                cap: self.current.line_cap,
                join: self.current.line_join,
                miter_limit: self.current.miter_limit,
                dash: self.current.dash.clone(),
            },
            paint: Paint::Solid(self.current.stroke_color),
            alpha: self.current.stroke_alpha,
        });
    }

    fn paint_fill(&mut self, rule: FillRule) {
        let path = std::mem::take(&mut self.current_path);
        if path.is_empty() {
            return;
        }
        self.display_list.push(RenderCommand::FillPath {
            path,
            rule,
            paint: Paint::Solid(self.current.fill_color),
            alpha: self.current.fill_alpha,
        });
    }

    fn paint_fill_then_stroke(&mut self, rule: FillRule) {
        let path = std::mem::take(&mut self.current_path);
        if path.is_empty() {
            return;
        }
        self.display_list.push(RenderCommand::FillPath {
            path: path.clone(),
            rule,
            paint: Paint::Solid(self.current.fill_color),
            alpha: self.current.fill_alpha,
        });
        self.display_list.push(RenderCommand::StrokePath {
            path,
            style: StrokeStyle {
                width: self.current.line_width,
                cap: self.current.line_cap,
                join: self.current.line_join,
                miter_limit: self.current.miter_limit,
                dash: self.current.dash.clone(),
            },
            paint: Paint::Solid(self.current.stroke_color),
            alpha: self.current.stroke_alpha,
        });
    }

    fn show_text(&mut self, bytes: &[u8]) {
        let tm = self.text_matrix;
        let ctm = self.current.ctm;
        let combined = ctm.concat(&tm);

        let font_size = self.current.font_size;
        let h_scale = self.current.h_scaling / 100.0;

        let font_and_id = self.current_font_id.and_then(|fid| {
            self.font_cache
                .and_then(|fc| fc.get(fid).map(|f| (fid, f)))
        });

        // Determine if 2-byte (CID) encoding
        let is_two_byte = font_and_id
            .map(|(_, f)| matches!(f.font_type, zpdf_font::PdfFontType::Type0CidType2))
            .unwrap_or(bytes.len() % 2 == 0 && bytes.iter().any(|&b| b > 127));

        let advance_divisor = font_and_id
            .map(|(_, f)| f.advance_divisor())
            .unwrap_or(1000.0);

        let scale_factor = font_size / advance_divisor as f32;

        let mut glyphs = Vec::new();
        let mut x_offset = 0.0f32;

        if is_two_byte {
            for chunk in bytes.chunks(2) {
                let glyph_id = if chunk.len() == 2 {
                    ((chunk[0] as u16) << 8) | chunk[1] as u16
                } else {
                    chunk[0] as u16
                };
                let advance = if let Some((_, font)) = font_and_id {
                    font.glyph_advance(glyph_id) as f32 * scale_factor * h_scale
                } else {
                    font_size * 0.5 * h_scale
                };
                glyphs.push(PositionedGlyph {
                    glyph_id,
                    x: x_offset,
                    y: 0.0,
                    advance,
                });
                x_offset += advance + self.current.char_spacing;
            }
        } else {
            for &byte in bytes {
                let glyph_id = byte as u16;
                let advance = if let Some((_, font)) = font_and_id {
                    font.glyph_advance(glyph_id) as f32 * scale_factor * h_scale
                } else {
                    font_size * 0.6 * h_scale
                };
                glyphs.push(PositionedGlyph {
                    glyph_id,
                    x: x_offset,
                    y: 0.0,
                    advance,
                });
                x_offset += advance + self.current.char_spacing;
                if byte == b' ' {
                    x_offset += self.current.word_spacing;
                }
            }
        }

        let font_id = self.current_font_id.unwrap_or(0);

        if !glyphs.is_empty() {
            self.display_list
                .push(RenderCommand::DrawGlyphRun(GlyphRun {
                    font_id,
                    font_size,
                    glyphs,
                    paint: Paint::Solid(self.current.fill_color),
                    alpha: self.current.fill_alpha,
                    transform: combined,
                }));
        }

        // Advance text matrix: x_offset is in user/text space units
        let advance = Matrix::translate(x_offset as f64, 0.0);
        self.text_matrix = advance.concat(&self.text_matrix);
    }

    fn adjust_text_position(&mut self, amount: f64) {
        // TJ displacement: amount is in thousandths of a unit of text space
        let displacement = amount / 1000.0 * self.current.font_size as f64;
        let advance = Matrix::translate(displacement, 0.0);
        self.text_matrix = self.text_matrix.concat(&advance);
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

    #[test]
    fn interpret_text_block() {
        let content = b"BT /F1 12 Tf 100 700 Td (Hello World) Tj ET";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let dl = ContentInterpreter::new(page_rect).interpret(content);
        assert_eq!(dl.commands.len(), 1);
        assert!(matches!(&dl.commands[0], RenderCommand::DrawGlyphRun(_)));
    }

    #[test]
    fn interpret_tj_array() {
        let content = b"BT /F1 12 Tf 100 700 Td [(AB) -200 (CD)] TJ ET";
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let dl = ContentInterpreter::new(page_rect).interpret(content);
        assert_eq!(dl.commands.len(), 2); // two glyph runs
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
}
