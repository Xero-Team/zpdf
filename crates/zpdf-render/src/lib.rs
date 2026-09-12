pub mod dash;

use zpdf_core::Rect;
use zpdf_display_list::{Color, DisplayList, RenderCommand};

/// Render configuration for a page.
pub struct PageRenderInfo {
    pub page_rect: Rect,
    pub scale: f32,
    pub background: Color,
}

/// Invalid page geometry supplied to a render backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageGeometryError {
    NonFinite,
    NonPositiveScale,
    NonPositiveBounds,
    RasterTooLarge,
}

impl std::fmt::Display for PageGeometryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFinite => f.write_str("page bounds, scale, and background must be finite"),
            Self::NonPositiveScale => f.write_str("render scale must be greater than zero"),
            Self::NonPositiveBounds => {
                f.write_str("page width and height must be greater than zero")
            }
            Self::RasterTooLarge => f.write_str("scaled raster dimensions exceed u32 limits"),
        }
    }
}

impl std::error::Error for PageGeometryError {}

impl PageRenderInfo {
    /// Validate the common page inputs and return ceil-rounded raster dimensions.
    /// Backends call this before allocating so NaN/infinite or inverted boxes cannot
    /// turn into saturating casts and unexpectedly huge allocations.
    pub fn raster_dimensions(&self) -> Result<(u32, u32), PageGeometryError> {
        let rect = self.page_rect;
        let finite = [
            rect.x0,
            rect.y0,
            rect.x1,
            rect.y1,
            self.scale as f64,
            self.background.r as f64,
            self.background.g as f64,
            self.background.b as f64,
            self.background.a as f64,
        ]
        .into_iter()
        .all(f64::is_finite);
        if !finite {
            return Err(PageGeometryError::NonFinite);
        }
        if self.scale <= 0.0 {
            return Err(PageGeometryError::NonPositiveScale);
        }
        if [rect.x0, rect.y0, rect.x1, rect.y1]
            .into_iter()
            .any(|v| v.abs() > f32::MAX as f64)
        {
            return Err(PageGeometryError::RasterTooLarge);
        }
        let width = rect.width();
        let height = rect.height();
        if width <= 0.0 || height <= 0.0 {
            return Err(PageGeometryError::NonPositiveBounds);
        }
        let width = (width * self.scale as f64).ceil();
        let height = (height * self.scale as f64).ceil();
        if !width.is_finite()
            || !height.is_finite()
            || width > u32::MAX as f64
            || height > u32::MAX as f64
        {
            return Err(PageGeometryError::RasterTooLarge);
        }
        Ok(((width as u32).max(1), (height as u32).max(1)))
    }
}

/// Opt-in per-stage timings for one rendered page.
///
/// **Off by default.** Backends accumulate these only when explicitly asked
/// (`CpuRenderer::with_stage_timing`, `WgpuRenderer::with_gpu_timing`), so the
/// production path pays neither clock reads nor counter increments. A benchmark
/// that measures a backend with this enabled is measuring a *slightly slower*
/// path than a consumer sees — which is why the benches report the timing-off
/// run as the headline number and this as a labelled diagnostic.
///
/// Buckets are keyed by the **work performed**, not by display-list command, so
/// two properties matter when reading them:
///
/// * **Work nests.** A soft-mask group rasterizes a sub-display-list, and
///   overprint/knockout re-paints a shape; those calls land in the buckets of
///   the work they do. One command can therefore contribute to several buckets,
///   and the buckets need not sum to [`StageStats::total_ns`]. Treat
///   `total_ns` as the denominator and the buckets as attribution.
/// * **`shading` is deliberately absent.** Shadings are rasterized by the
///   *interpreter* (`zpdf-content/shading.rs::rasterize`) and reach a backend as
///   an ordinary [`RenderCommand::DrawImage`] — by the time the backend sees
///   one it is indistinguishable from a bitmap image. Measure shading in the
///   interpret stage (see `zpdf_content::InterpretStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageStats {
    /// Extracting glyph outlines from the font program — `glyf` / CFF
    /// charstring interpretation. This is the term §10.1 of the performance
    /// notes hypothesised dominates *cold* rendering (warm/cold was measured at
    /// 20x) and never isolated from rasterization.
    pub outline_parse_ns: u64,
    /// Turning those outlines into device paths and rasterizing them.
    pub glyph_raster_ns: u64,
    /// Solid-path fills.
    pub fill_ns: u64,
    /// Solid-path strokes.
    pub stroke_ns: u64,
    /// Image blits and resampling — the largest single measured cost on the
    /// image-bound page (3 full-page bilinear upsamples, ~5.4 s).
    pub image_ns: u64,
    /// Clip mask construction and intersection (`push_clip` /
    /// `push_clip_stroke` / `pop_clip`).
    pub clip_ns: u64,
    /// Soft-mask rasterization and blend-group compositing.
    ///
    /// The whole soft-mask cost. [`StageStats::mask_render_ns`],
    /// [`StageStats::mask_reduce_ns`], [`StageStats::mask_fold_ns`] and
    /// [`StageStats::mask_composite_ns`] attribute parts of it, and their sum is
    /// **≤** this total — not equal to it. The remainder is plane shifting
    /// (`shift_plane`, a full-plane copy when a mask is reused at an offset) and
    /// cache/key maintenance, which are real but small next to the four measured
    /// terms. A strict partition was claimed here first and measurement
    /// disproved it (~60% unaccounted), which is exactly why the sub-buckets
    /// exist.
    pub soft_mask_ns: u64,
    /// Attribution: re-rendering the mask group's commands into a full-page
    /// scratch raster, including that raster's allocation and backdrop fill.
    pub mask_render_ns: u64,
    /// Attribution: reducing the scratch raster to a 1-byte coverage plane
    /// (per-pixel demultiply + Rec.601 luma + /TR LUT).
    pub mask_reduce_ns: u64,
    /// Attribution: folding a plane into a group's pixels (per-pixel per-channel
    /// multiply/divide over the full raster).
    pub mask_fold_ns: u64,
    /// Attribution: compositing the finished group onto its backdrop —
    /// `draw_pixmap` with the group's blend mode and constant alpha, over the
    /// full raster. Non-`SourceOver` modes (HSL family especially) cost far more
    /// than the default, so this term is not proportional to the others.
    pub mask_composite_ns: u64,
    /// Destination pixels the group composite actually *covers* (Σ of each
    /// group's non-transparent bounding-box area), against `groups × page area`
    /// for the full-raster composite.
    ///
    /// Diagnostic for whether scoping the composite to the group's extent is
    /// worthwhile: if group content covers most of the page, it is not. Counted
    /// in destination pixels rather than nanoseconds so it is deterministic and
    /// CI-gateable — it answers "does the approach apply" independently of how
    /// fast this machine happens to be.
    pub mask_composite_px: u64,
    /// Whole-page wall time, the reference denominator for the buckets above.
    pub total_ns: u64,

    // --- counters: integer, deterministic, and therefore CI-gateable ---
    /// Glyph instances drawn (cache hits included).
    pub glyphs: u64,
    /// Calls that actually extracted an outline from the font program. Lower
    /// than [`StageStats::glyphs`] whenever a cache or reuse avoids the work.
    pub glyph_outlines_parsed: u64,
    pub fills: u64,
    pub strokes: u64,
    pub images: u64,
    pub clips_pushed: u64,
    pub soft_mask_planes: u64,
}

impl StageStats {
    /// Glyph work as one number (outline extraction + rasterization).
    pub fn glyph_ns(&self) -> u64 {
        self.outline_parse_ns.saturating_add(self.glyph_raster_ns)
    }

    /// `part` as a fraction of [`StageStats::total_ns`], or 0.0 when the total
    /// is unknown (timing disabled) — so a caller can print a share without
    /// guarding every division.
    pub fn share(&self, part: u64) -> f64 {
        if self.total_ns == 0 {
            0.0
        } else {
            part as f64 / self.total_ns as f64
        }
    }

    /// True when nothing was recorded — distinguish "timing off" from "measured
    /// zero", which a `Default` value alone cannot convey.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Backend-agnostic render trait.
///
/// Implementations: zpdf-render-cpu (tiny-skia), zpdf-render-wgpu (GPU).
pub trait RenderBackend {
    type Target;
    type Error: std::error::Error;

    fn begin_page(&mut self, info: &PageRenderInfo) -> Result<(), Self::Error>;
    fn execute(&mut self, cmd: &RenderCommand) -> Result<(), Self::Error>;
    fn end_page(&mut self) -> Result<Self::Target, Self::Error>;

    fn render_display_list(
        &mut self,
        dl: &DisplayList,
        scale: f32,
    ) -> Result<Self::Target, Self::Error> {
        self.begin_page(&PageRenderInfo {
            page_rect: dl.page_rect,
            scale,
            background: Color::white(),
        })?;
        for cmd in &dl.commands {
            self.execute(cmd)?;
        }
        self.end_page()
    }

    /// Per-stage timings for the most recently rendered page, or `None` when
    /// this backend was not configured to collect them.
    ///
    /// Collection is opt-in because it puts clock reads on a hot path; see
    /// [`StageStats`]. The default implementation reports "not collecting", so
    /// adding the hook cost existing backends nothing.
    fn stage_stats(&self) -> Option<StageStats> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(rect: Rect, scale: f32) -> PageRenderInfo {
        PageRenderInfo {
            page_rect: rect,
            scale,
            background: Color::white(),
        }
    }

    #[test]
    fn dimensions_are_ceil_rounded() {
        assert_eq!(
            info(Rect::new(0.0, 0.0, 595.0, 842.0), 110.0 / 72.0).raster_dimensions(),
            Ok((910, 1287))
        );
    }

    #[test]
    fn rejects_invalid_geometry_before_casting() {
        assert_eq!(
            info(Rect::new(0.0, 0.0, f64::INFINITY, 10.0), 1.0).raster_dimensions(),
            Err(PageGeometryError::NonFinite)
        );
        assert_eq!(
            info(Rect::new(0.0, 0.0, 10.0, 10.0), f32::NAN).raster_dimensions(),
            Err(PageGeometryError::NonFinite)
        );
        assert_eq!(
            info(Rect::new(0.0, 0.0, 10.0, 10.0), 0.0).raster_dimensions(),
            Err(PageGeometryError::NonPositiveScale)
        );
    }

    #[test]
    fn stage_stats_default_is_empty() {
        assert!(StageStats::default().is_empty());
        // A single non-zero field is enough to make it "measured something".
        let stats = StageStats {
            fill_ns: 1,
            ..Default::default()
        };
        assert!(!stats.is_empty());
    }

    #[test]
    fn stage_stats_share_handles_zero_total() {
        // Zero total means "timing off"; share must not divide by zero or
        // report a misleading 0 % as if it were measured.
        let stats = StageStats {
            fill_ns: 500,
            ..Default::default()
        };
        assert_eq!(stats.share(500), 0.0);

        let stats = StageStats {
            fill_ns: 500,
            total_ns: 1000,
            ..Default::default()
        };
        assert_eq!(stats.share(500), 0.5);
    }

    #[test]
    fn mask_sub_buckets_are_attribution_not_a_partition() {
        // The sub-buckets attribute *parts* of `soft_mask_ns`; the remainder is
        // plane shifting and cache upkeep. Assert the documented direction
        // (sum <= total) rather than an equality that measurement showed false.
        let stats = StageStats {
            soft_mask_ns: 1000,
            mask_render_ns: 100,
            mask_reduce_ns: 200,
            mask_fold_ns: 300,
            mask_composite_ns: 350,
            ..Default::default()
        };
        let attributed = stats.mask_render_ns
            + stats.mask_reduce_ns
            + stats.mask_fold_ns
            + stats.mask_composite_ns;
        assert!(
            attributed <= stats.soft_mask_ns,
            "attribution {attributed} must not exceed the total {}",
            stats.soft_mask_ns
        );
    }

    #[test]
    fn glyph_ns_covers_both_glyph_terms() {
        let stats = StageStats {
            outline_parse_ns: 700,
            glyph_raster_ns: 300,
            ..Default::default()
        };
        assert_eq!(stats.glyph_ns(), 1000);
    }

    /// A backend that does not opt in must report `None`, not `Some(default)`,
    /// so a caller can tell "not collecting" from "collected zero".
    #[test]
    fn stage_stats_hook_defaults_to_not_collecting() {
        struct Silent;
        impl RenderBackend for Silent {
            type Target = ();
            type Error = PageGeometryError;
            fn begin_page(&mut self, _: &PageRenderInfo) -> Result<(), Self::Error> {
                Ok(())
            }
            fn execute(&mut self, _: &RenderCommand) -> Result<(), Self::Error> {
                Ok(())
            }
            fn end_page(&mut self) -> Result<(), Self::Error> {
                Ok(())
            }
        }
        assert_eq!(Silent.stage_stats(), None);
    }
}
