//! PDF color space definitions and conversion.

pub mod function;
pub mod icc;

pub use function::PdfFunction;
pub use icc::{IccCache, IccTransform};

#[derive(Debug, Clone)]
pub enum ColorSpace {
    DeviceGray,
    DeviceRGB,
    DeviceCMYK,
    CalGray(CalGrayParams),
    CalRGB(CalRGBParams),
    Lab(LabParams),
    ICCBased(std::sync::Arc<IccTransform>),
    Indexed {
        base: Box<ColorSpace>,
        max_index: u8,
        lookup: Vec<u8>,
    },
    Separation {
        name: String,
        alternate: Box<ColorSpace>,
    },
    DeviceN {
        names: Vec<String>,
        alternate: Box<ColorSpace>,
    },
    Pattern,
}

#[derive(Debug, Clone)]
pub struct CalGrayParams {
    pub white_point: [f64; 3],
    pub black_point: [f64; 3],
    pub gamma: f64,
}

#[derive(Debug, Clone)]
pub struct CalRGBParams {
    pub white_point: [f64; 3],
    pub black_point: [f64; 3],
    pub gamma: [f64; 3],
    pub matrix: [f64; 9],
}

#[derive(Debug, Clone)]
pub struct LabParams {
    pub white_point: [f64; 3],
    pub black_point: [f64; 3],
    pub range: [f64; 4],
}

/// Convert CMYK values to RGB (simple approximation).
pub fn cmyk_to_rgb(c: f64, m: f64, y: f64, k: f64) -> (f64, f64, f64) {
    let r = (1.0 - c) * (1.0 - k);
    let g = (1.0 - m) * (1.0 - k);
    let b = (1.0 - y) * (1.0 - k);
    (r, g, b)
}

/// Convert a CIE L*a*b* color to sRGB (ISO 32000-1 §8.6.5.4).
///
/// `white_point` is the diffuse white from the Lab dict (default D50,
/// `[0.9505, 1.0, 1.089]` is D65; PDF writers commonly use D50
/// `[0.9643, 1.0, 0.8251]`). Inputs are the raw L (0..100), a, b values
/// after /Range clamping; output channels are clamped to 0..1.
pub fn lab_to_rgb(l: f64, a: f64, b: f64, white_point: [f64; 3]) -> (f64, f64, f64) {
    // Lab -> XYZ
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let g = |t: f64| {
        if t >= 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f64 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    let x = white_point[0] * g(fx);
    let y = white_point[1] * g(fy);
    let z = white_point[2] * g(fz);
    xyz_to_srgb(x, y, z)
}

/// Convert CIE XYZ (D50-ish white) to gamma-encoded sRGB, clamped to 0..1.
pub fn xyz_to_srgb(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    // sRGB D65 matrix; the small white-point mismatch vs D50 sources is
    // acceptable without full chromatic adaptation.
    let r = 3.2406 * x - 1.5372 * y - 0.4986 * z;
    let g = -0.9689 * x + 1.8758 * y + 0.0415 * z;
    let b = 0.0557 * x - 0.2040 * y + 1.0570 * z;
    let enc = |c: f64| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    (enc(r), enc(g), enc(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmyk_black() {
        let (r, g, b) = cmyk_to_rgb(0.0, 0.0, 0.0, 1.0);
        assert!((r - 0.0).abs() < 1e-10);
        assert!((g - 0.0).abs() < 1e-10);
        assert!((b - 0.0).abs() < 1e-10);
    }

    #[test]
    fn cmyk_white() {
        let (r, g, b) = cmyk_to_rgb(0.0, 0.0, 0.0, 0.0);
        assert!((r - 1.0).abs() < 1e-10);
        assert!((g - 1.0).abs() < 1e-10);
        assert!((b - 1.0).abs() < 1e-10);
    }
}
