#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn zero() -> Self {
        Self { x: 0.0, y: 0.0 }
    }

    pub fn transform(self, m: &Matrix) -> Self {
        Self {
            x: m.a * self.x + m.c * self.y + m.e,
            y: m.b * self.x + m.d * self.y + m.f,
        }
    }
}

/// PDF rectangle: [x0, y0, x1, y1] (lower-left, upper-right).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { x0, y0, x1, y1 }
    }

    pub fn width(&self) -> f64 {
        (self.x1 - self.x0).abs()
    }

    pub fn height(&self) -> f64 {
        (self.y1 - self.y0).abs()
    }

    pub fn normalize(&self) -> Self {
        Self {
            x0: self.x0.min(self.x1),
            y0: self.y0.min(self.y1),
            x1: self.x0.max(self.x1),
            y1: self.y0.max(self.y1),
        }
    }
}

/// 3x2 affine transformation matrix.
///
/// PDF uses the representation `[a b c d e f]` which maps:
///   x' = a*x + c*y + e
///   y' = b*x + d*y + f
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Matrix {
    pub fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    pub fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Self { a, b, c, d, e, f }
    }

    pub fn translate(tx: f64, ty: f64) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }

    pub fn scale(sx: f64, sy: f64) -> Self {
        Self {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Matrix multiply: self * other.
    ///
    /// Represents applying `other` first, then `self`.
    /// PDF `cm` semantics: new_CTM = old_CTM.concat(cm_matrix).
    pub fn concat(&self, other: &Matrix) -> Self {
        // Layout: [a c e; b d f; 0 0 1]
        Self {
            a: self.a * other.a + self.c * other.b,
            b: self.b * other.a + self.d * other.b,
            c: self.a * other.c + self.c * other.d,
            d: self.b * other.c + self.d * other.d,
            e: self.a * other.e + self.c * other.f + self.e,
            f: self.b * other.e + self.d * other.f + self.f,
        }
    }

    pub fn determinant(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }

    pub fn inverse(&self) -> Option<Self> {
        let det = self.determinant();
        if det.abs() < 1e-12 {
            return None;
        }
        let inv_det = 1.0 / det;
        Some(Self {
            a: self.d * inv_det,
            b: -self.b * inv_det,
            c: -self.c * inv_det,
            d: self.a * inv_det,
            e: (self.c * self.f - self.e * self.d) * inv_det,
            f: (self.e * self.b - self.a * self.f) * inv_det,
        })
    }
}

impl Default for Matrix {
    fn default() -> Self {
        Self::identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_concat() {
        let id = Matrix::identity();
        let m = Matrix::new(2.0, 0.0, 0.0, 3.0, 10.0, 20.0);
        assert_eq!(id.concat(&m), m);
        assert_eq!(m.concat(&id), m);
    }

    #[test]
    fn inverse_roundtrip() {
        let m = Matrix::new(2.0, 1.0, -1.0, 3.0, 5.0, 7.0);
        let inv = m.inverse().unwrap();
        let result = m.concat(&inv);
        let id = Matrix::identity();
        assert!((result.a - id.a).abs() < 1e-10);
        assert!((result.d - id.d).abs() < 1e-10);
        assert!((result.e - id.e).abs() < 1e-10);
    }

    #[test]
    fn point_transform() {
        let m = Matrix::translate(10.0, 20.0);
        let p = Point::new(1.0, 2.0);
        let t = p.transform(&m);
        assert!((t.x - 11.0).abs() < 1e-10);
        assert!((t.y - 22.0).abs() < 1e-10);
    }

    #[test]
    fn rect_normalize() {
        let r = Rect::new(10.0, 20.0, 5.0, 8.0);
        let n = r.normalize();
        assert_eq!(n, Rect::new(5.0, 8.0, 10.0, 20.0));
    }
}
