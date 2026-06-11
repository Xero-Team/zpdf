//! ICC profile parsing and profile→sRGB transforms.
//!
//! Wraps the pure-Rust `moxcms` CMS behind crate-local types so the rest of
//! the workspace never names the CMS crate. A profile that fails to parse or
//! cannot be connected to sRGB is an `Err` from [`IccTransform::from_profile_bytes`]
//! (and a cached `None` in [`IccCache`]); callers fall back to the
//! component-count behaviour instead of failing the render.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use moxcms::{
    ColorProfile, DataColorSpace, Layout, RenderingIntent, Transform8BitExecutor, TransformOptions,
};
use zpdf_core::{Error, ObjectId, Result};

/// A compiled ICC-profile → sRGB transform for 1/3/4-component input.
///
/// The underlying executor is `Send + Sync`, so a transform can be shared
/// across threads behind its usual `Arc`.
pub struct IccTransform {
    ncomp: u8,
    executor: Arc<Transform8BitExecutor>,
}

impl fmt::Debug for IccTransform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IccTransform")
            .field("ncomp", &self.ncomp)
            .finish_non_exhaustive()
    }
}

impl IccTransform {
    /// Parse an ICC profile and compile its device→sRGB transform.
    ///
    /// Supported data colour spaces: Gray (1 component), RGB and Lab (3),
    /// CMYK (4). Anything else — including malformed or truncated profiles —
    /// is an error so the caller can keep its `/N`-based fallback.
    pub fn from_profile_bytes(data: &[u8]) -> Result<Self> {
        let profile = ColorProfile::new_from_slice(data)
            .map_err(|e| Error::StreamDecode(format!("ICC profile parse failed: {e:?}")))?;
        let (layout, ncomp) = match profile.color_space {
            DataColorSpace::Gray => (Layout::Gray, 1u8),
            // Lab data profiles are 3-channel; raster samples use the ICC
            // 8-bit Lab encoding, which the CMS handles internally.
            DataColorSpace::Rgb | DataColorSpace::Lab => (Layout::Rgb, 3),
            // moxcms convention: 8-bit CMYK shares the 4-channel Rgba layout.
            DataColorSpace::Cmyk => (Layout::Rgba, 4),
            other => {
                return Err(Error::StreamDecode(format!(
                    "unsupported ICC data colour space {other:?}"
                )))
            }
        };
        let srgb = ColorProfile::new_srgb();
        // TODO(/RenderingIntent): plumb the PDF rendering intent through to
        // here. For now use media-relative colorimetric, retrying perceptual
        // for LUT profiles that only carry an A2B0 table (the ICC-mandated
        // fallback order).
        let executor = [
            RenderingIntent::RelativeColorimetric,
            RenderingIntent::Perceptual,
        ]
        .into_iter()
        .find_map(|intent| {
            profile
                .create_transform_8bit(
                    layout,
                    &srgb,
                    Layout::Rgb,
                    TransformOptions {
                        rendering_intent: intent,
                        ..TransformOptions::default()
                    },
                )
                .ok()
        })
        .ok_or_else(|| {
            Error::StreamDecode("ICC profile cannot be connected to sRGB".to_string())
        })?;
        Ok(Self { ncomp, executor })
    }

    /// Input components per colour (1, 3, or 4).
    pub fn components(&self) -> usize {
        self.ncomp as usize
    }

    /// Convert one colour with components in 0..=1 to sRGB in 0..=1.
    ///
    /// Components are quantized to 8 bits — the same precision as the raster
    /// path. Missing components read as 0.
    pub fn color_to_rgb(&self, comps: &[f64]) -> (f64, f64, f64) {
        let mut src = [0u8; 4];
        for (i, s) in src.iter_mut().enumerate().take(self.components()) {
            let v = comps.get(i).copied().unwrap_or(0.0);
            *s = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        let [r, g, b] = self.comps8_to_rgb8(&src);
        (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
    }

    /// Convert one colour of 8-bit components (first `components()` entries
    /// used) to 8-bit sRGB.
    pub fn comps8_to_rgb8(&self, comps: &[u8; 4]) -> [u8; 3] {
        let mut dst = [0u8; 3];
        // The executor only errs on slice-length mismatches, which the fixed
        // sizes here rule out; keep black as the impossible-case fallback.
        let _ = self.executor.transform(&comps[..self.components()], &mut dst);
        dst
    }

    /// Buffer-level conversion of interleaved `components()`-byte samples to
    /// interleaved 8-bit RGB. Both slices must describe the same pixel count.
    pub fn slice_to_rgb(&self, src: &[u8], dst: &mut [u8]) -> Result<()> {
        self.executor
            .transform(src, dst)
            .map_err(|e| Error::StreamDecode(format!("ICC transform failed: {e:?}")))
    }

    /// Convert a palette of `components()`-byte entries into 3-byte RGB
    /// entries (bakes Indexed-with-ICC-base lookup tables at resolve time).
    /// Trailing bytes that do not form a whole entry are dropped.
    pub fn palette_to_rgb(&self, palette: &[u8]) -> Vec<u8> {
        let n = self.components();
        let entries = palette.len() / n;
        let mut out = vec![0u8; entries * 3];
        if let Err(e) = self.slice_to_rgb(&palette[..entries * n], &mut out) {
            tracing::warn!("ICC palette conversion failed: {e}");
        }
        out
    }
}

/// Per-document cache of ICCBased profile streams → compiled transforms,
/// keyed by the profile stream's object id. Failures are cached as `None`
/// so a malformed profile is parsed (and warned about) only once.
#[derive(Debug, Default)]
pub struct IccCache {
    transforms: HashMap<ObjectId, Option<Arc<IccTransform>>>,
}

impl IccCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached transform for profile stream `id`, building it from the
    /// bytes returned by `data` on first use. `data` returning `None`
    /// (unresolvable stream) also caches as a failure.
    pub fn get_or_build(
        &mut self,
        id: ObjectId,
        data: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Option<Arc<IccTransform>> {
        self.transforms
            .entry(id)
            .or_insert_with(|| {
                let bytes = data()?;
                match IccTransform::from_profile_bytes(&bytes) {
                    Ok(t) => Some(Arc::new(t)),
                    Err(e) => {
                        tracing::warn!(
                            "ICC profile {id}: {e}; using component-count colour fallback"
                        );
                        None
                    }
                }
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRGB: &[u8] = include_bytes!("testdata/srgb.icc");
    const GRAY_GAMMA22: &[u8] = include_bytes!("testdata/gray_gamma22.icc");
    const GRAY_LINEAR: &[u8] = include_bytes!("testdata/gray_linear.icc");
    const CMYK_LUT: &[u8] = include_bytes!("testdata/cmyk_lut.icc");

    #[test]
    fn srgb_profile_is_identity() {
        let t = IccTransform::from_profile_bytes(SRGB).unwrap();
        assert_eq!(t.components(), 3);
        let mut out = [0u8; 6];
        t.slice_to_rgb(&[10, 128, 240, 0, 255, 64], &mut out).unwrap();
        for (a, b) in out.iter().zip([10u8, 128, 240, 0, 255, 64]) {
            assert!((*a as i16 - b as i16).abs() <= 2, "not identity: {out:?}");
        }
    }

    #[test]
    fn srgb_float_color_roundtrips() {
        let t = IccTransform::from_profile_bytes(SRGB).unwrap();
        let (r, g, b) = t.color_to_rgb(&[1.0, 0.0, 0.0]);
        assert!(r > 0.98 && g < 0.02 && b < 0.02, "got {r} {g} {b}");
    }

    #[test]
    fn gray_gamma22_tone_curve_applies() {
        // A gamma-2.2 gray curve is close to (but not exactly) sRGB's
        // transfer: 128 → encode(0.502^2.2) ≈ 129.
        let t = IccTransform::from_profile_bytes(GRAY_GAMMA22).unwrap();
        assert_eq!(t.components(), 1);
        let mut out = [0u8; 9];
        t.slice_to_rgb(&[0, 128, 255], &mut out).unwrap();
        assert_eq!(&out[0..3], &[0, 0, 0]);
        assert!((out[3] as i16 - 129).abs() <= 2, "midtone: {out:?}");
        assert_eq!(&out[6..9], &[255, 255, 255]);
    }

    #[test]
    fn gray_linear_brightens_midtones() {
        // Linear gray re-encoded with the sRGB curve: 128 → ≈188, a visible
        // departure from the old pass-through (which would keep 128).
        let t = IccTransform::from_profile_bytes(GRAY_LINEAR).unwrap();
        let mut out = [0u8; 3];
        t.slice_to_rgb(&[128], &mut out).unwrap();
        assert!((out[0] as i16 - 188).abs() <= 2, "midtone: {out:?}");
    }

    #[test]
    fn cmyk_lut_profile_converts_through_lut() {
        let t = IccTransform::from_profile_bytes(CMYK_LUT).unwrap();
        assert_eq!(t.components(), 4);
        // white, black (K only), cyan
        let src = [0u8, 0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0];
        let mut out = [0u8; 9];
        t.slice_to_rgb(&src, &mut out).unwrap();
        assert!(out[0] > 220 && out[1] > 220 && out[2] > 220, "white: {out:?}");
        assert!(out[3] < 30 && out[4] < 30 && out[5] < 30, "black: {out:?}");
        assert!(out[6] < 60 && out[7] > 180 && out[8] > 180, "cyan: {out:?}");
    }

    #[test]
    fn adobe_rgb_differs_from_passthrough() {
        // A wide-gamut profile must visibly move saturated colours; built
        // with moxcms here so no large fixture is needed.
        let bytes = moxcms::ColorProfile::new_adobe_rgb().encode().unwrap();
        let t = IccTransform::from_profile_bytes(&bytes).unwrap();
        let mut out = [0u8; 3];
        t.slice_to_rgb(&[60, 200, 60], &mut out).unwrap();
        assert!(
            (out[0] as i16 - 60).abs() > 30,
            "expected a visible shift, got {out:?}"
        );
    }

    #[test]
    fn malformed_profile_is_rejected() {
        assert!(IccTransform::from_profile_bytes(&[0u8; 256]).is_err());
        assert!(IccTransform::from_profile_bytes(&SRGB[..100]).is_err());
        assert!(IccTransform::from_profile_bytes(b"").is_err());
    }

    #[test]
    fn cache_remembers_failures_without_reparsing() {
        let mut cache = IccCache::new();
        let id = ObjectId(7, 0);
        let mut calls = 0;
        for _ in 0..2 {
            let t = cache.get_or_build(id, || {
                calls += 1;
                Some(vec![0u8; 64])
            });
            assert!(t.is_none());
        }
        assert_eq!(calls, 1, "failure was not cached");
    }

    #[test]
    fn cache_shares_one_transform_per_id() {
        let mut cache = IccCache::new();
        let id = ObjectId(3, 0);
        let a = cache.get_or_build(id, || Some(SRGB.to_vec())).unwrap();
        let b = cache.get_or_build(id, || unreachable!()).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn palette_bakes_through_transform() {
        let t = IccTransform::from_profile_bytes(GRAY_LINEAR).unwrap();
        let rgb = t.palette_to_rgb(&[0, 128, 255]);
        assert_eq!(rgb.len(), 9);
        assert_eq!(&rgb[0..3], &[0, 0, 0]);
        assert!((rgb[3] as i16 - 188).abs() <= 2);
        assert_eq!(&rgb[6..9], &[255, 255, 255]);
    }
}
