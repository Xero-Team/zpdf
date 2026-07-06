//! Ink annotation builder (ISO 32000-1 §12.5.6.13).
//!
//! An ink annotation represents freeform "handwritten" scribbles or graffiti on
//! a PDF page. This module provides an API to construct ink annotations from a
//! series of strokes (polylines in page space) and serialize them into the PDF
//! dictionary + appearance stream format required by the specification.

use zpdf_core::Rect;

/// Builder for ink annotations. Accumulates strokes (each stroke is a polyline
/// of `(x, y)` points in page space, origin bottom-left) and produces a PDF
/// annotation dictionary plus its appearance stream.
#[derive(Debug, Clone)]
pub struct InkAnnotationBuilder {
    /// The ink strokes (`/InkList`): an array of paths, where each path is a
    /// sequence of `(x, y)` points in page space.
    ink_list: Vec<Vec<(f64, f64)>>,
    /// Stroke color (DeviceRGB, 0.0–1.0 per component).
    color: (f64, f64, f64),
    /// Line width in points.
    width: f64,
}

impl InkAnnotationBuilder {
    /// Create a new builder with default settings (black ink, 1pt width).
    pub fn new() -> Self {
        Self {
            ink_list: Vec::new(),
            color: (0.0, 0.0, 0.0), // black
            width: 1.0,
        }
    }

    /// Add a stroke (a polyline of `(x, y)` points in page space, origin
    /// bottom-left, Y+ upward). Each point is in PDF user-space units (1/72 inch).
    /// At least two points are needed to form a line; single-point or empty
    /// strokes are silently dropped.
    pub fn add_stroke(&mut self, points: Vec<(f64, f64)>) {
        if points.len() >= 2 {
            self.ink_list.push(points);
        }
    }

    /// Set the stroke color (DeviceRGB). Each component is in the range [0.0, 1.0].
    pub fn set_color(&mut self, r: f64, g: f64, b: f64) {
        self.color = (r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0));
    }

    /// Set the line width in points.
    pub fn set_width(&mut self, w: f64) {
        self.width = w.max(0.1);
    }

    /// Compute the bounding rectangle from all strokes, with a small margin to
    /// account for the line width. Returns `None` if there are no strokes.
    pub fn compute_rect(&self) -> Option<Rect> {
        if self.ink_list.is_empty() {
            return None;
        }
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;

        for stroke in &self.ink_list {
            for &(x, y) in stroke {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }

        // Add margin: half the line width on each side, plus a 1pt safety buffer.
        let margin = self.width / 2.0 + 1.0;
        Some(Rect {
            x0: min_x - margin,
            y0: min_y - margin,
            x1: max_x + margin,
            y1: max_y + margin,
        })
    }

    /// Build the annotation dictionary and appearance stream. Returns:
    /// - A PDF dictionary (the annotation object's content, as key-value pairs)
    /// - The appearance stream bytes (a PDF content stream for `/AP /N`)
    ///
    /// Returns `None` if there are no strokes (nothing to serialize).
    ///
    /// The caller is responsible for:
    /// - Wrapping the dict in an indirect object (e.g., `5 0 obj <dict> endobj`)
    /// - Wrapping the appearance bytes in a stream object with the correct header
    /// - Assigning object numbers and wiring `/AP /N` to reference the stream
    pub fn build(&self) -> Option<(InkAnnotDict, Vec<u8>)> {
        let rect = self.compute_rect()?;

        // The annotation dictionary fields.
        let dict = InkAnnotDict {
            rect,
            ink_list: self.ink_list.clone(),
            color: self.color,
            width: self.width,
        };

        // The appearance stream (PDF content operators).
        let appearance = self.build_appearance_stream(&rect);

        Some((dict, appearance))
    }

    /// Generate the PDF content stream for the appearance (`/AP /N`). The stream
    /// draws each stroke as a path with `m` (moveto) + `l` (lineto) + `S` (stroke).
    fn build_appearance_stream(&self, _rect: &Rect) -> Vec<u8> {
        let mut stream = Vec::new();
        let (r, g, b) = self.color;

        // The appearance XObject has its own coordinate system: the annotation's
        // `/Rect` becomes the XObject's bounding box (`/BBox`), so we don't need
        // to offset coordinates — they're already in the right space.
        //
        // Content: q <width> w <r g b> RG <strokes> Q
        stream.extend_from_slice(b"q\n");
        stream.extend_from_slice(format!("{:.3} w\n", self.width).as_bytes());
        stream.extend_from_slice(format!("{:.3} {:.3} {:.3} RG\n", r, g, b).as_bytes());

        for stroke in &self.ink_list {
            if let Some(&(x0, y0)) = stroke.first() {
                stream.extend_from_slice(format!("{:.2} {:.2} m\n", x0, y0).as_bytes());
                for &(x, y) in &stroke[1..] {
                    stream.extend_from_slice(format!("{:.2} {:.2} l\n", x, y).as_bytes());
                }
                stream.extend_from_slice(b"S\n");
            }
        }

        stream.extend_from_slice(b"Q\n");
        stream
    }
}

impl Default for InkAnnotationBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// The fields of an ink annotation dictionary, ready for serialization.
#[derive(Debug, Clone)]
pub struct InkAnnotDict {
    /// The annotation's bounding rectangle (`/Rect`).
    pub rect: Rect,
    /// The ink paths (`/InkList`): an array of arrays of numbers.
    pub ink_list: Vec<Vec<(f64, f64)>>,
    /// The stroke color (`/C`), DeviceRGB.
    pub color: (f64, f64, f64),
    /// The border width (`/BS /W`).
    pub width: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_rect_includes_all_points_with_margin() {
        let mut builder = InkAnnotationBuilder::new();
        builder.set_width(2.0);
        builder.add_stroke(vec![(10.0, 20.0), (30.0, 40.0)]);
        builder.add_stroke(vec![(5.0, 15.0), (35.0, 45.0)]);

        let rect = builder.compute_rect().expect("rect");
        // min: (5, 15), max: (35, 45), margin = 2/2 + 1 = 2
        assert_eq!(rect.x0, 3.0);
        assert_eq!(rect.y0, 13.0);
        assert_eq!(rect.x1, 37.0);
        assert_eq!(rect.y1, 47.0);
    }

    #[test]
    fn single_point_strokes_are_dropped() {
        let mut builder = InkAnnotationBuilder::new();
        builder.add_stroke(vec![(10.0, 20.0)]); // single point
        builder.add_stroke(vec![]); // empty
        assert!(builder.compute_rect().is_none());
    }

    #[test]
    fn build_produces_dict_and_appearance() {
        let mut builder = InkAnnotationBuilder::new();
        builder.set_color(1.0, 0.0, 0.0); // red
        builder.set_width(3.0);
        builder.add_stroke(vec![(100.0, 200.0), (150.0, 250.0)]);

        let (dict, appearance) = builder.build().expect("build");
        assert_eq!(dict.color, (1.0, 0.0, 0.0));
        assert_eq!(dict.width, 3.0);
        assert_eq!(dict.ink_list.len(), 1);

        // The appearance stream must contain the stroke color and path operators.
        let s = String::from_utf8_lossy(&appearance);
        assert!(s.contains("1.000 0.000 0.000 RG")); // red stroke color
        assert!(s.contains("3.000 w")); // line width
        assert!(s.contains("100.00 200.00 m")); // moveto
        assert!(s.contains("150.00 250.00 l")); // lineto
        assert!(s.contains("S")); // stroke
    }

    #[test]
    fn color_clamped_to_valid_range() {
        let mut builder = InkAnnotationBuilder::new();
        builder.set_color(-0.5, 1.5, 0.5);
        assert_eq!(builder.color, (0.0, 1.0, 0.5));
    }

    #[test]
    fn width_has_minimum() {
        let mut builder = InkAnnotationBuilder::new();
        builder.set_width(0.0);
        assert_eq!(builder.width, 0.1);
        builder.set_width(-5.0);
        assert_eq!(builder.width, 0.1);
    }
}
