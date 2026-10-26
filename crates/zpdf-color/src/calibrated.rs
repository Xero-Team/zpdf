//! Calibrated colour to display sRGB (ISO 32000-1 8.6.5.2/8.6.5.3).
use crate::{adapt_xyz_to_d65, xyz_to_srgb, CalGrayParams, CalRGBParams};
use zpdf_core::{ObjectId, PdfDict, PdfObject};

const D65: [f64; 3] = [0.95047, 1.0, 1.08883];
const IDENTITY: [f64; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

fn resolved(
    obj: &PdfObject,
    resolve: &mut dyn FnMut(ObjectId) -> Option<PdfObject>,
) -> Option<PdfObject> {
    let mut value = obj.clone();
    for _ in 0..16 {
        match value {
            PdfObject::Ref(id) => value = resolve(id)?,
            _ => return Some(value),
        }
    }
    None
}

fn array<const N: usize>(
    dict: &PdfDict,
    key: &str,
    resolve: &mut dyn FnMut(ObjectId) -> Option<PdfObject>,
) -> Option<[f64; N]> {
    let object = resolved(dict.get(key)?, resolve)?;
    let values = object.as_array().ok()?;
    if values.len() != N {
        return None;
    }
    let mut result = [0.0; N];
    for (out, value) in result.iter_mut().zip(values) {
        *out = resolved(value, resolve)?.as_f64().ok()?;
        if !out.is_finite() {
            return None;
        }
    }
    Some(result)
}

fn valid_white(w: [f64; 3]) -> [f64; 3] {
    if w.iter().all(|v| v.is_finite() && *v > 0.0) && (w[1] - 1.0).abs() < 1e-6 {
        w
    } else {
        D65
    }
}
fn gamma(g: f64) -> f64 {
    if g.is_finite() && g > 0.0 {
        g
    } else {
        1.0
    }
}
fn unit(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn to_display(mut xyz: [f64; 3], white: [f64; 3], black: [f64; 3]) -> (f64, f64, f64) {
    // Relative black-point compensation: source diffuse black maps to display
    // black, diffuse white stays fixed. Bradford adaptation then maps source
    // white to the D65 display white, followed by the sRGB transfer function.
    for i in 0..3 {
        let b = if black[i].is_finite() && black[i] >= 0.0 && black[i] < white[i] {
            black[i]
        } else {
            0.0
        };
        xyz[i] = (xyz[i] - b).max(0.0) * white[i] / (white[i] - b);
    }
    let (x, y, z) = adapt_xyz_to_d65(xyz[0], xyz[1], xyz[2], white);
    xyz_to_srgb(x, y, z)
}

impl CalGrayParams {
    /// Parse direct or indirect parameters with bounded reference resolution.
    /// Malformed values use the standard optional defaults and D65 for white.
    pub fn from_pdf_dict(
        dict: &PdfDict,
        resolve: &mut dyn FnMut(ObjectId) -> Option<PdfObject>,
    ) -> Self {
        Self {
            white_point: valid_white(array(dict, "WhitePoint", resolve).unwrap_or(D65)),
            black_point: array(dict, "BlackPoint", resolve).unwrap_or([0.0; 3]),
            gamma: gamma(
                dict.get("Gamma")
                    .and_then(|o| resolved(o, resolve))
                    .and_then(|o| o.as_f64().ok())
                    .unwrap_or(1.0),
            ),
        }
    }
    pub fn to_rgb(&self, gray: f64) -> (f64, f64, f64) {
        let white = valid_white(self.white_point);
        let a = unit(gray).powf(gamma(self.gamma));
        to_display(white.map(|v| v * a), white, self.black_point)
    }
}

impl CalRGBParams {
    /// Parse calibrated RGB parameters; Matrix is column-major as in PDF.
    pub fn from_pdf_dict(
        dict: &PdfDict,
        resolve: &mut dyn FnMut(ObjectId) -> Option<PdfObject>,
    ) -> Self {
        Self {
            white_point: valid_white(array(dict, "WhitePoint", resolve).unwrap_or(D65)),
            black_point: array(dict, "BlackPoint", resolve).unwrap_or([0.0; 3]),
            gamma: array(dict, "Gamma", resolve).unwrap_or([1.0; 3]).map(gamma),
            matrix: array(dict, "Matrix", resolve).unwrap_or(IDENTITY),
        }
    }
    pub fn to_rgb(&self, rgb: [f64; 3]) -> (f64, f64, f64) {
        let abc = std::array::from_fn::<_, 3, _>(|i| unit(rgb[i]).powf(gamma(self.gamma[i])));
        let m = if self.matrix.iter().all(|v| v.is_finite()) {
            self.matrix
        } else {
            IDENTITY
        };
        let xyz = std::array::from_fn(|i| m[i] * abc[0] + m[i + 3] * abc[1] + m[i + 6] * abc[2]);
        to_display(xyz, valid_white(self.white_point), self.black_point)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cyclic_parameter_references_are_bounded() {
        let mut dict = PdfDict::new();
        dict.insert(
            zpdf_core::PdfName::new("Gamma"),
            PdfObject::Ref(ObjectId(1, 0)),
        );
        let mut calls = 0;
        let params = CalGrayParams::from_pdf_dict(&dict, &mut |id| {
            calls += 1;
            Some(PdfObject::Ref(id))
        });
        assert_eq!(calls, 16);
        assert_eq!(params.gamma, 1.0);
    }
    #[test]
    fn gray_gamma_and_white_adaptation_have_known_srgb_values() {
        let mut p = CalGrayParams {
            white_point: [0.9642, 1.0, 0.8249],
            black_point: [0.0; 3],
            gamma: 1.0,
        };
        let rgb = p.to_rgb(0.5);
        for v in [rgb.0, rgb.1, rgb.2] {
            assert!((v - 0.73536).abs() < 0.001);
        }
        p.gamma = 2.0;
        let rgb = p.to_rgb(0.5);
        for v in [rgb.0, rgb.1, rgb.2] {
            assert!((v - 0.5371).abs() < 0.001);
        }
        p.black_point = p.white_point.map(|v| v * 0.25);
        let rgb = p.to_rgb(0.5);
        assert_eq!(rgb, (0.0, 0.0, 0.0));
    }
    #[test]
    fn rgb_matrix_is_column_major_and_malformed_values_stay_finite() {
        let mut p = CalRGBParams {
            white_point: D65,
            black_point: [0.0; 3],
            gamma: [1.0; 3],
            matrix: [
                0.4124, 0.2126, 0.0193, 0.3576, 0.7152, 0.1192, 0.1805, 0.0722, 0.9505,
            ],
        };
        let (r, g, b) = p.to_rgb([1.0, 0.0, 0.0]);
        assert!(r > 0.999 && g < 0.001 && b < 0.001);
        p.gamma = [f64::NAN; 3];
        p.matrix = [f64::INFINITY; 9];
        p.white_point = [0.0; 3];
        let (r, g, b) = p.to_rgb([f64::NAN, f64::INFINITY, -1.0]);
        assert!([r, g, b]
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
    }
}
