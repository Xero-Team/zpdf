//! PDF color space definitions and conversion.

pub mod function;
pub mod icc;

pub use function::PdfFunction;
pub use icc::{IccCache, IccTransform, RenderIntent};

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

/// Convert DeviceCMYK to gamma-encoded sRGB (each channel in `0.0..=1.0`).
///
/// Uses the Adobe DeviceCMYK→sRGB polynomial approximation (fitted to US Web
/// Coated SWOP, the same one Acrobat and pdf.js use), which is far closer to a
/// reference renderer than the naïve `(1−c)(1−k)`: it accounts for ink
/// impurity, so e.g. 100 % K renders as a dark near-black `(0.17, 0.18, 0.21)`
/// rather than pure black, and 100 % C as `(0, 0.72, 0.95)` rather than pure
/// `(0, 1, 1)`. This is the non-ICC path; DeviceCMYK with an embedded/Default
/// ICC profile goes through [`IccTransform`] instead. Inputs are clamped to
/// `0.0..=1.0`.
///
/// Coefficients: Mozilla pdf.js `DeviceCmykCS` (Apache-2.0), in turn the Adobe
/// approximation.
pub fn cmyk_to_rgb(c: f64, m: f64, y: f64, k: f64) -> (f64, f64, f64) {
    let c = c.clamp(0.0, 1.0);
    let m = m.clamp(0.0, 1.0);
    let y = y.clamp(0.0, 1.0);
    let k = k.clamp(0.0, 1.0);

    // Each channel is a quadratic form over (c, m, y, k), evaluated in 0..255.
    let r = 255.0
        + c * (-4.387332384609988 * c
            + 54.48615194189176 * m
            + 18.82290502165302 * y
            + 212.25662451639585 * k
            - 285.2331026137004)
        + m * (1.7149763477362134 * m
            - 5.6096736904047315 * y
            - 17.873870861415444 * k
            - 5.497006427196366)
        + y * (-2.5217340131683033 * y - 21.248923337353073 * k + 17.5119270841813)
        + k * (-21.86122147463605 * k - 189.48180835922747);

    let g = 255.0
        + c * (8.841041422036149 * c
            + 60.118027045597366 * m
            + 6.871425592049007 * y
            + 31.159100130055922 * k
            - 79.2970844816548)
        + m * (-15.310361306967817 * m + 17.575251261109482 * y + 131.35250912493976 * k
            - 190.9453302588951)
        + y * (4.444339102852739 * y + 9.8632861493405 * k - 24.86741582555878)
        + k * (-20.737325471181034 * k - 187.80453709719578);

    let b = 255.0
        + c * (0.8842522430003296 * c + 8.078677503112928 * m + 30.89978309703729 * y
            - 0.23883238689178934 * k
            - 14.183576799673286)
        + m * (10.49593273432072 * m + 63.02378494754052 * y + 50.606957656360734 * k
            - 112.23884253719248)
        + y * (0.03296041114873217 * y + 115.60384449646641 * k - 193.58209356861505)
        + k * (-22.33816807309886 * k - 180.12613974708367);

    (
        (r / 255.0).clamp(0.0, 1.0),
        (g / 255.0).clamp(0.0, 1.0),
        (b / 255.0).clamp(0.0, 1.0),
    )
}

// Naïve subtractive CMYK ↔ sRGB (mutually-inverse, used for overprint
// compositing) live in `zpdf-core` so both render backends — which do not
// depend on this crate — share one definition. Re-exported here for the
// content interpreter and colour-path callers.
pub use zpdf_core::{cmyk_to_rgb_naive, rgb_to_cmyk_naive};

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

    /// Round the conversion to 8-bit RGB, as the renderers ultimately do.
    fn u8s(c: f64, m: f64, y: f64, k: f64) -> (u8, u8, u8) {
        let (r, g, b) = cmyk_to_rgb(c, m, y, k);
        (
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8,
        )
    }

    #[test]
    fn cmyk_white_is_pure_white() {
        let (r, g, b) = cmyk_to_rgb(0.0, 0.0, 0.0, 0.0);
        assert!((r - 1.0).abs() < 1e-10);
        assert!((g - 1.0).abs() < 1e-10);
        assert!((b - 1.0).abs() < 1e-10);
    }

    #[test]
    fn cmyk_reference_points_match_adobe_polynomial() {
        // Reference sRGB from the Adobe DeviceCMYK approximation (pdf.js parity).
        // 100 % K is a dark near-black, NOT pure black — ink impurity (SWOP).
        assert_eq!(u8s(0.0, 0.0, 0.0, 1.0), (44, 46, 53)); // black (K only)
        assert_eq!(u8s(1.0, 0.0, 0.0, 0.0), (0, 185, 242)); // cyan
        assert_eq!(u8s(0.0, 1.0, 0.0, 0.0), (251, 49, 153)); // magenta
        assert_eq!(u8s(0.0, 0.0, 1.0, 0.0), (255, 235, 61)); // yellow
        assert_eq!(u8s(1.0, 1.0, 1.0, 1.0), (6, 6, 12)); // registration black
    }

    #[test]
    fn cmyk_clamps_out_of_range_without_panicking() {
        // Out-of-gamut / out-of-range inputs are clamped, output stays in 0..1.
        let (r, g, b) = cmyk_to_rgb(2.0, -1.0, 0.5, 5.0);
        for v in [r, g, b] {
            assert!((0.0..=1.0).contains(&v));
        }
        // Clamped == the in-range equivalent.
        assert_eq!(
            cmyk_to_rgb(2.0, -1.0, 0.5, 5.0),
            cmyk_to_rgb(1.0, 0.0, 0.5, 1.0)
        );
    }
}
