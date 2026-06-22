//! Naïve subtractive CMYK ↔ sRGB, a mutually-inverse pair.
//!
//! These live in `zpdf-core` so the content interpreter, the colour crate, and
//! both render backends share one definition. They model the textbook
//! `r = (1−c)(1−k)` ink behaviour, ignoring ink impurity — so 100 % K is pure
//! black. That makes them **exact inverses** of each other, which is the
//! property [overprint] (PDF 8.6.7) compositing relies on: a colorant an
//! overprint leaves untouched must come back out of the backdrop unchanged.
//!
//! Normal DeviceCMYK *painting* uses the fidelity SWOP polynomial
//! (`zpdf_color::cmyk_to_rgb`) instead; this naïve pair is for overprint only.
//!
//! [overprint]: https://opensource.adobe.com/dc-acrobat-sdk-docs/pdfstandards/PDF32000_2008.pdf

/// Naïve subtractive CMYK → sRGB: `r=(1−c)(1−k)`, `g=(1−m)(1−k)`, `b=(1−y)(1−k)`.
/// The exact inverse of [`rgb_to_cmyk_naive`]. Inputs clamped to `0.0..=1.0`.
pub fn cmyk_to_rgb_naive(c: f64, m: f64, y: f64, k: f64) -> (f64, f64, f64) {
    let c = c.clamp(0.0, 1.0);
    let m = m.clamp(0.0, 1.0);
    let y = y.clamp(0.0, 1.0);
    let k = k.clamp(0.0, 1.0);
    (
        (1.0 - c) * (1.0 - k),
        (1.0 - m) * (1.0 - k),
        (1.0 - y) * (1.0 - k),
    )
}

/// Naïve sRGB → subtractive CMYK with maximum GCR (`k = 1 − max(r,g,b)`), the
/// exact inverse of [`cmyk_to_rgb_naive`]. Pure black/white map to
/// `(0,0,0,1)` / `(0,0,0,0)`. Inputs clamped to `0.0..=1.0`.
pub fn rgb_to_cmyk_naive(r: f64, g: f64, b: f64) -> (f64, f64, f64, f64) {
    let r = r.clamp(0.0, 1.0);
    let g = g.clamp(0.0, 1.0);
    let b = b.clamp(0.0, 1.0);
    let k = 1.0 - r.max(g).max(b);
    if k >= 1.0 {
        (0.0, 0.0, 0.0, 1.0)
    } else {
        let inv = 1.0 - k;
        (
            (1.0 - r - k) / inv,
            (1.0 - g - k) / inv,
            (1.0 - b - k) / inv,
            k,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naive_cmyk_roundtrips_exactly() {
        // rgb→cmyk→rgb is the identity — the property overprint relies on.
        for &(r, g, b) in &[
            (1.0, 1.0, 0.0),
            (0.0, 1.0, 1.0),
            (1.0, 0.0, 1.0),
            (0.2, 0.6, 0.9),
            (0.0, 0.0, 0.0),
            (1.0, 1.0, 1.0),
            (0.5, 0.5, 0.5),
        ] {
            let (c, m, y, k) = rgb_to_cmyk_naive(r, g, b);
            let (r2, g2, b2) = cmyk_to_rgb_naive(c, m, y, k);
            assert!(
                (r - r2).abs() < 1e-9 && (g - g2).abs() < 1e-9 && (b - b2).abs() < 1e-9,
                "roundtrip ({r},{g},{b}) -> ({r2},{g2},{b2})"
            );
        }
    }

    #[test]
    fn naive_cmyk_reference_points() {
        assert_eq!(rgb_to_cmyk_naive(1.0, 1.0, 0.0), (0.0, 0.0, 1.0, 0.0)); // yellow
        assert_eq!(rgb_to_cmyk_naive(0.0, 0.0, 0.0), (0.0, 0.0, 0.0, 1.0)); // black
        assert_eq!(rgb_to_cmyk_naive(1.0, 1.0, 1.0), (0.0, 0.0, 0.0, 0.0)); // white
        assert_eq!(cmyk_to_rgb_naive(0.0, 0.0, 0.0, 1.0), (0.0, 0.0, 0.0)); // K=1 → black
                                                                            // Cyan-active over yellow's colorants (0,0,1,0) → (1,0,1,0) → green:
                                                                            // the hallmark of overprint.
        assert_eq!(cmyk_to_rgb_naive(1.0, 0.0, 1.0, 0.0), (0.0, 1.0, 0.0));
    }
}
