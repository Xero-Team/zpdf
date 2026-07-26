//! Shape recognition from PDF paths.
//!
//! Attempts to recognize common geometric shapes (rectangles, ellipses, lines)
//! from arbitrary PDF paths to produce cleaner PowerPoint output.

use zpdf_core::{Point, Rect};
use zpdf_display_list::{Path, PathElement};

/// Recognize if a path is a simple rectangle.
pub fn recognize_rectangle(path: &Path) -> Option<Rect> {
    if path.elements.len() != 5 {
        return None;
    }

    let mut corners = Vec::new();

    for elem in &path.elements {
        match elem {
            PathElement::MoveTo(p) => {
                corners.push(*p);
            }
            PathElement::LineTo(p) => {
                corners.push(*p);
            }
            PathElement::Close => {}
            _ => return None, // Curves disqualify it as a rectangle
        }
    }

    // Should have MoveTo + 3 LineTo + Close = 4 corners
    if corners.len() != 4 {
        return None;
    }

    // Check if corners form a rectangle (axis-aligned)
    let xs: Vec<f64> = corners.iter().map(|p| p.x).collect();
    let ys: Vec<f64> = corners.iter().map(|p| p.y).collect();

    let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().copied().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    // Check that we have exactly 2 unique X values and 2 unique Y values
    let unique_xs: std::collections::HashSet<_> = xs.iter().map(|x| (*x * 100.0) as i64).collect();
    let unique_ys: std::collections::HashSet<_> = ys.iter().map(|y| (*y * 100.0) as i64).collect();

    if unique_xs.len() == 2 && unique_ys.len() == 2 {
        Some(Rect::new(min_x, min_y, max_x, max_y))
    } else {
        None
    }
}

/// Recognize if a path is an ellipse (approximate check using bounding box).
pub fn recognize_ellipse(path: &Path) -> Option<Rect> {
    // An ellipse in PDF is typically drawn with 4 cubic Béziers
    // This is a simplified check - count curves and estimate bounds
    let mut curve_count = 0;
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    let mut has_move = false;
    let mut has_close = false;

    for elem in &path.elements {
        match elem {
            PathElement::MoveTo(p) => {
                has_move = true;
                min_x = min_x.min(p.x);
                max_x = max_x.max(p.x);
                min_y = min_y.min(p.y);
                max_y = max_y.max(p.y);
            }
            PathElement::CurveTo(c1, c2, end) => {
                curve_count += 1;
                for p in [c1, c2, end] {
                    min_x = min_x.min(p.x);
                    max_x = max_x.max(p.x);
                    min_y = min_y.min(p.y);
                    max_y = max_y.max(p.y);
                }
            }
            PathElement::Close => {
                has_close = true;
            }
            _ => return None, // Lines disqualify it as an ellipse
        }
    }

    // Typical ellipse has 4 cubic Bézier curves
    if curve_count == 4 && has_move && has_close {
        Some(Rect::new(min_x, min_y, max_x, max_y))
    } else {
        None
    }
}

/// Recognize if a path is a simple line.
pub fn recognize_line(path: &Path) -> Option<(Point, Point)> {
    if path.elements.len() != 2 {
        return None;
    }

    match (&path.elements[0], &path.elements[1]) {
        (PathElement::MoveTo(p1), PathElement::LineTo(p2)) => Some((*p1, *p2)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_simple_rectangle() {
        let mut path = Path::new();
        path.move_to(Point::new(0.0, 0.0));
        path.line_to(Point::new(100.0, 0.0));
        path.line_to(Point::new(100.0, 50.0));
        path.line_to(Point::new(0.0, 50.0));
        path.close();

        let rect = recognize_rectangle(&path);
        assert!(rect.is_some());
        let r = rect.unwrap();
        assert_eq!(r.x0, 0.0);
        assert_eq!(r.y0, 0.0);
        assert_eq!(r.x1, 100.0);
        assert_eq!(r.y1, 50.0);
    }

    #[test]
    fn recognizes_simple_line() {
        let mut path = Path::new();
        path.move_to(Point::new(10.0, 20.0));
        path.line_to(Point::new(30.0, 40.0));

        let line = recognize_line(&path);
        assert!(line.is_some());
    }

    #[test]
    fn rejects_non_rectangle() {
        let mut path = Path::new();
        path.move_to(Point::new(0.0, 0.0));
        path.line_to(Point::new(100.0, 0.0));
        path.line_to(Point::new(50.0, 50.0)); // Triangle
        path.close();

        let rect = recognize_rectangle(&path);
        assert!(rect.is_none());
    }
}
