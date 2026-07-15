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

const MAX_ICC_PROFILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ICC_CACHE_ENTRIES: usize = 1024;

/// PDF colour rendering intent (ISO 32000-1 §8.6.5.8) — the `ri` operator,
/// ExtGState `/RI`, and image `/Intent`. Defaults to media-relative
/// colorimetric, the PDF default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RenderIntent {
    Perceptual,
    #[default]
    RelativeColorimetric,
    Saturation,
    AbsoluteColorimetric,
}

impl RenderIntent {
    /// Map a PDF intent name to an intent. Unknown names fall back to the
    /// media-relative colorimetric default (per the spec's recommendation).
    pub fn from_pdf_name(name: &str) -> Self {
        match name {
            "Perceptual" => Self::Perceptual,
            "Saturation" => Self::Saturation,
            "AbsoluteColorimetric" => Self::AbsoluteColorimetric,
            _ => Self::RelativeColorimetric,
        }
    }

    fn to_moxcms(self) -> RenderingIntent {
        match self {
            Self::Perceptual => RenderingIntent::Perceptual,
            Self::RelativeColorimetric => RenderingIntent::RelativeColorimetric,
            Self::Saturation => RenderingIntent::Saturation,
            Self::AbsoluteColorimetric => RenderingIntent::AbsoluteColorimetric,
        }
    }
}

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
    pub fn from_profile_bytes(data: &[u8], intent: RenderIntent) -> Result<Self> {
        if data.len() < 128 || data.len() > MAX_ICC_PROFILE_BYTES {
            return Err(Error::StreamDecode(format!(
                "ICC profile size {} is outside 128..={MAX_ICC_PROFILE_BYTES}",
                data.len()
            )));
        }
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
        // Try the requested PDF rendering intent first, then fall back through
        // media-relative colorimetric and perceptual — the ICC-mandated order
        // for LUT profiles that only carry a subset of A2B tables.
        let intents = [
            intent.to_moxcms(),
            RenderingIntent::RelativeColorimetric,
            RenderingIntent::Perceptual,
        ];
        let executor = intents
            .into_iter()
            .enumerate()
            .filter(|(index, candidate)| !intents[..*index].contains(candidate))
            .find_map(|(_, intent)| {
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
        let _ = self
            .executor
            .transform(&comps[..self.components()], &mut dst);
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
        let Some(output_len) = entries.checked_mul(3) else {
            return Vec::new();
        };
        let mut out = vec![0u8; output_len];
        if let Err(e) = self.slice_to_rgb(&palette[..entries * n], &mut out) {
            tracing::warn!("ICC palette conversion failed: {e}");
        }
        out
    }
}

/// Per-document cache of ICCBased profile streams → compiled transforms,
/// keyed by the profile stream's object id AND the rendering intent (the same
/// profile may be requested under different intents on one page). Failures are
/// cached as `None` so a malformed profile is parsed (and warned about) once.
#[derive(Debug, Default)]
pub struct IccCache {
    transforms: HashMap<(ObjectId, RenderIntent), Option<Arc<IccTransform>>>,
}

impl IccCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached transform for profile stream `id` under `intent`, building it
    /// from the bytes returned by `data` on first use. `data` returning `None`
    /// (unresolvable stream) also caches as a failure.
    pub fn get_or_build(
        &mut self,
        id: ObjectId,
        intent: RenderIntent,
        data: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Option<Arc<IccTransform>> {
        if let Some(cached) = self.transforms.get(&(id, intent)) {
            return cached.clone();
        }
        if self.transforms.len() >= MAX_ICC_CACHE_ENTRIES {
            tracing::warn!(
                "ICC transform cache reached {MAX_ICC_CACHE_ENTRIES} entries; ignoring profile {id}"
            );
            return None;
        }
        self.transforms
            .entry((id, intent))
            .or_insert_with(|| {
                let bytes = data()?;
                match IccTransform::from_profile_bytes(&bytes, intent) {
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

    /// The default media-relative colorimetric intent, for tests that don't
    /// exercise intent selection.
    fn ri() -> RenderIntent {
        RenderIntent::default()
    }

    #[test]
    fn srgb_profile_is_identity() {
        let t = IccTransform::from_profile_bytes(SRGB, ri()).unwrap();
        assert_eq!(t.components(), 3);
        let mut out = [0u8; 6];
        t.slice_to_rgb(&[10, 128, 240, 0, 255, 64], &mut out)
            .unwrap();
        for (a, b) in out.iter().zip([10u8, 128, 240, 0, 255, 64]) {
            assert!((*a as i16 - b as i16).abs() <= 2, "not identity: {out:?}");
        }
    }

    #[test]
    fn srgb_float_color_roundtrips() {
        let t = IccTransform::from_profile_bytes(SRGB, ri()).unwrap();
        let (r, g, b) = t.color_to_rgb(&[1.0, 0.0, 0.0]);
        assert!(r > 0.98 && g < 0.02 && b < 0.02, "got {r} {g} {b}");
    }

    #[test]
    fn gray_gamma22_tone_curve_applies() {
        // A gamma-2.2 gray curve is close to (but not exactly) sRGB's
        // transfer: 128 → encode(0.502^2.2) ≈ 129.
        let t = IccTransform::from_profile_bytes(GRAY_GAMMA22, ri()).unwrap();
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
        let t = IccTransform::from_profile_bytes(GRAY_LINEAR, ri()).unwrap();
        let mut out = [0u8; 3];
        t.slice_to_rgb(&[128], &mut out).unwrap();
        assert!((out[0] as i16 - 188).abs() <= 2, "midtone: {out:?}");
    }

    #[test]
    fn cmyk_lut_profile_converts_through_lut() {
        let t = IccTransform::from_profile_bytes(CMYK_LUT, ri()).unwrap();
        assert_eq!(t.components(), 4);
        // white, black (K only), cyan
        let src = [0u8, 0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0];
        let mut out = [0u8; 9];
        t.slice_to_rgb(&src, &mut out).unwrap();
        assert!(
            out[0] > 220 && out[1] > 220 && out[2] > 220,
            "white: {out:?}"
        );
        assert!(out[3] < 30 && out[4] < 30 && out[5] < 30, "black: {out:?}");
        assert!(out[6] < 60 && out[7] > 180 && out[8] > 180, "cyan: {out:?}");
    }

    #[test]
    fn adobe_rgb_differs_from_passthrough() {
        // A wide-gamut profile must visibly move saturated colours; built
        // with moxcms here so no large fixture is needed.
        let bytes = moxcms::ColorProfile::new_adobe_rgb().encode().unwrap();
        let t = IccTransform::from_profile_bytes(&bytes, ri()).unwrap();
        let mut out = [0u8; 3];
        t.slice_to_rgb(&[60, 200, 60], &mut out).unwrap();
        assert!(
            (out[0] as i16 - 60).abs() > 30,
            "expected a visible shift, got {out:?}"
        );
    }

    #[test]
    fn render_intent_name_mapping() {
        use RenderIntent::*;
        assert_eq!(RenderIntent::from_pdf_name("Perceptual"), Perceptual);
        assert_eq!(
            RenderIntent::from_pdf_name("RelativeColorimetric"),
            RelativeColorimetric
        );
        assert_eq!(RenderIntent::from_pdf_name("Saturation"), Saturation);
        assert_eq!(
            RenderIntent::from_pdf_name("AbsoluteColorimetric"),
            AbsoluteColorimetric
        );
        // Unknown / unspecified → media-relative colorimetric default.
        assert_eq!(RenderIntent::from_pdf_name("Bogus"), RelativeColorimetric);
        assert_eq!(RenderIntent::default(), RelativeColorimetric);
    }

    #[test]
    fn every_intent_compiles_a_working_transform() {
        use RenderIntent::*;
        for intent in [
            Perceptual,
            RelativeColorimetric,
            Saturation,
            AbsoluteColorimetric,
        ] {
            let t = IccTransform::from_profile_bytes(SRGB, intent)
                .unwrap_or_else(|e| panic!("intent {intent:?} failed: {e}"));
            let mut out = [0u8; 3];
            t.slice_to_rgb(&[200, 50, 50], &mut out).unwrap();
            // sRGB→sRGB stays near identity for every intent (no gamut clip).
            assert!((out[0] as i16 - 200).abs() <= 4, "{intent:?}: {out:?}");
        }
    }

    #[test]
    fn cache_separates_transforms_by_intent() {
        let mut cache = IccCache::new();
        let id = ObjectId(11, 0);
        let mut calls = 0;
        let _a = cache.get_or_build(id, RenderIntent::Perceptual, || {
            calls += 1;
            Some(SRGB.to_vec())
        });
        // A different intent must NOT reuse the perceptual entry.
        let _b = cache.get_or_build(id, RenderIntent::AbsoluteColorimetric, || {
            calls += 1;
            Some(SRGB.to_vec())
        });
        assert_eq!(calls, 2, "intents must be cached separately");
        // Same (id, intent) reuses.
        let _c = cache.get_or_build(id, RenderIntent::Perceptual, || unreachable!());
    }

    #[test]
    fn malformed_profile_is_rejected() {
        assert!(IccTransform::from_profile_bytes(&[0u8; 256], ri()).is_err());
        assert!(IccTransform::from_profile_bytes(&SRGB[..100], ri()).is_err());
        assert!(IccTransform::from_profile_bytes(b"", ri()).is_err());
    }

    #[test]
    fn cache_remembers_failures_without_reparsing() {
        let mut cache = IccCache::new();
        let id = ObjectId(7, 0);
        let mut calls = 0;
        for _ in 0..2 {
            let t = cache.get_or_build(id, ri(), || {
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
        let a = cache
            .get_or_build(id, ri(), || Some(SRGB.to_vec()))
            .unwrap();
        let b = cache.get_or_build(id, ri(), || unreachable!()).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn cache_entry_count_is_bounded() {
        let mut cache = IccCache::new();
        for number in 0..MAX_ICC_CACHE_ENTRIES {
            assert!(cache
                .get_or_build(ObjectId(number as u32, 0), ri(), || None)
                .is_none());
        }
        let mut called = false;
        assert!(cache
            .get_or_build(ObjectId(u32::MAX, 0), ri(), || {
                called = true;
                Some(SRGB.to_vec())
            })
            .is_none());
        assert!(!called);
        assert_eq!(cache.transforms.len(), MAX_ICC_CACHE_ENTRIES);
    }

    #[test]
    fn palette_bakes_through_transform() {
        let t = IccTransform::from_profile_bytes(GRAY_LINEAR, ri()).unwrap();
        let rgb = t.palette_to_rgb(&[0, 128, 255]);
        assert_eq!(rgb.len(), 9);
        assert_eq!(&rgb[0..3], &[0, 0, 0]);
        assert!((rgb[3] as i16 - 188).abs() <= 2);
        assert_eq!(&rgb[6..9], &[255, 255, 255]);
    }
}
