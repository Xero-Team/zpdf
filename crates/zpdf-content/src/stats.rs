//! Interpret-stage timings.
//!
//! Separate from the render backends' `zpdf_render::StageStats` for one
//! structural reason: **shading rasterization happens here**. A PDF shading
//! (axial/radial gradient, or a type 4–7 mesh) is evaluated into a coverage
//! raster in [`crate::shading`] and handed to a backend as an ordinary
//! `DrawImage` — by the time a backend sees one it is indistinguishable from a
//! bitmap, so a backend cannot attribute the cost, and `Paint::Shading` never
//! reaches it as a paint (both render backends paint only `Paint::Solid`).
//! Every gradient on a page is therefore measurable only in this stage.
//!
//! Collected unconditionally, with no opt-in flag. The only clock reads are one
//! pair bracketing the whole interpret plus one pair per shading raster — a
//! handful per page. That is negligible against an interpret stage measured in
//! tens of milliseconds, unlike the render backends, where timing had to be
//! opt-in because it would sit inside per-primitive loops.

/// Per-page timings for the content-interpret stage (`parse` → `DisplayList`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterpretStats {
    /// Gradient/mesh evaluation into a raster — `shading::rasterize`.
    ///
    /// This is the whole cost of gradients on a page as far as any measurement
    /// can see it, and it lands on the CPU even when the GPU backend renders
    /// the page.
    pub shading_ns: u64,
    /// Whole-interpret wall time, the denominator for [`Self::share`].
    pub total_ns: u64,
    /// Shading rasters produced. Deterministic, therefore CI-gateable.
    pub shading_rasters: u64,
}

impl InterpretStats {
    /// `part` as a fraction of [`Self::total_ns`], or 0.0 when the total is
    /// unknown — so a caller can print a share without guarding the division.
    pub fn share(&self, part: u64) -> f64 {
        if self.total_ns == 0 {
            0.0
        } else {
            part as f64 / self.total_ns as f64
        }
    }

    /// True when nothing was recorded.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        assert!(InterpretStats::default().is_empty());
        let measured = InterpretStats {
            shading_rasters: 1,
            ..Default::default()
        };
        assert!(!measured.is_empty());
    }

    #[test]
    fn share_guards_zero_total() {
        let stats = InterpretStats {
            shading_ns: 400,
            ..Default::default()
        };
        assert_eq!(stats.share(400), 0.0);

        let stats = InterpretStats {
            shading_ns: 400,
            total_ns: 1600,
            ..Default::default()
        };
        assert_eq!(stats.share(400), 0.25);
    }
}
