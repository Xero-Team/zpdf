/// PDF color space definitions and conversion.

#[derive(Debug, Clone)]
pub enum ColorSpace {
    DeviceGray,
    DeviceRGB,
    DeviceCMYK,
    CalGray(CalGrayParams),
    CalRGB(CalRGBParams),
    Lab(LabParams),
    ICCBased(ICCProfile),
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

#[derive(Debug, Clone)]
pub struct ICCProfile {
    pub num_components: u8,
    pub data: Vec<u8>,
}

/// Convert CMYK values to RGB (simple approximation).
pub fn cmyk_to_rgb(c: f64, m: f64, y: f64, k: f64) -> (f64, f64, f64) {
    let r = (1.0 - c) * (1.0 - k);
    let g = (1.0 - m) * (1.0 - k);
    let b = (1.0 - y) * (1.0 - k);
    (r, g, b)
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
