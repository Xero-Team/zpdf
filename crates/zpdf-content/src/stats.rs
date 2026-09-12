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
//! # Two collection modes, on purpose
//!
//! [`InterpretStats::total_ns`] and [`InterpretStats::shading_ns`] are collected
//! **unconditionally**: the only clock reads are one pair bracketing the whole
//! interpret plus one pair per shading raster — a handful per page, negligible
//! against an interpret stage measured in tens of milliseconds.
//!
//! The per-category work buckets ([`InterpretStats::path_ns`] and friends) are
//! **opt-in** (`ContentInterpreter::with_work_timing`), because those clocks sit
//! on *every* attributed operator: a vector page can carry hundreds of thousands
//! of them, where the same reasoning that made the render backends' timing
//! opt-in applies. Off by default, so production pays one predictable branch and
//! no clock read.

/// Per-page timings for the content-interpret stage (`parse` → `DisplayList`).
///
/// The work buckets (`path_ns` … `color_ns`, and the `*_ops` counters) are
/// **attribution, not a partition**: nested work lands in more than one bucket
/// (a form's content is attributed to `form_ns` *and*, through the interpreter
/// re-entering its own operator loop, to whichever bucket each of its operators
/// belongs to), and the remainder — tokenizing plus every operator not worth a
/// clock read (`q`/`Q`/`gs`/state bookkeeping) — is attributed to nothing. Read
/// them against `total_ns` as a denominator, and expect their sum to be **≤**
/// the total. The earlier performance notes claimed a strict partition for the
/// render-side sub-buckets first and measurement disproved it; this one is
/// documented as attribution from the start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterpretStats {
    /// Gradient/mesh evaluation into a raster — `shading::rasterize`.
    ///
    /// This is the whole cost of gradients on a page as far as any measurement
    /// can see it, and it lands on the CPU even when the GPU backend renders
    /// the page.
    pub shading_ns: u64,
    /// Attribution: path-construction operators (`m`/`l`/`c`/`v`/`y`/`re`/`h`).
    pub path_ns: u64,
    /// Attribution: path-painting operators (`f`/`S`/`B`/`n`, plus `W`/`W*`).
    ///
    /// Since non-solid paints are resolved here (a tiling pattern expands to its
    /// tiles in the display list), this is where pattern expansion shows up.
    pub paint_ns: u64,
    /// Attribution: text operators (`BT`/`Tf`/`Td`/`Tj`/`TJ`/…) — glyph-run
    /// assembly, encoding/CMap lookup, and the text spans a consumer reads.
    pub text_ns: u64,
    /// Attribution: decoding an image XObject or an inline image, and admitting
    /// it to the image cache. Re-emitting an already-cached image is nearly free
    /// and lands here too.
    pub image_ns: u64,
    /// Attribution: interpreting a form XObject — including the `/G` group of a
    /// soft mask, which is a form drawn into a detached command list.
    pub form_ns: u64,
    /// Attribution: colour-space setting and component → RGB conversion
    /// (`cs`/`sc`/`scn`/`g`/`rg`/`k`), where an embedded ICC profile does its
    /// work.
    pub color_ns: u64,
    /// Whole-interpret wall time, the denominator for [`Self::share`].
    pub total_ns: u64,
    /// Shading rasters produced. Deterministic, therefore CI-gateable.
    pub shading_rasters: u64,
    /// Operators attributed, one counter per bucket. Deterministic: they say
    /// whether a page is dominated by *many cheap* operations or a few
    /// expensive ones, which the timings alone cannot.
    pub path_ops: u64,
    pub paint_ops: u64,
    pub text_ops: u64,
    pub image_ops: u64,
    pub form_ops: u64,
    pub color_ops: u64,
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

    /// One-line breakdown for a report: the work buckets in milliseconds with
    /// their share of the total, then the operator counts.
    ///
    /// A method rather than a bench-side format string so the CLI, a bench and a
    /// future consumer all describe a measurement the same way.
    pub fn describe(&self) -> String {
        let ms = |ns: u64| ns as f64 / 1e6;
        let pct = |ns: u64| self.share(ns) * 100.0;
        format!(
            "interpret total={:.2}ms (shading={:.2} {:.0}%) | \
             text={:.2} {:.0}% image={:.2} {:.0}% form={:.2} {:.0}% \
             paint={:.2} {:.0}% path={:.2} {:.0}% color={:.2} {:.0}% | \
             ops: text={} paint={} path={} image={} form={} color={} shading={}",
            ms(self.total_ns),
            ms(self.shading_ns),
            pct(self.shading_ns),
            ms(self.text_ns),
            pct(self.text_ns),
            ms(self.image_ns),
            pct(self.image_ns),
            ms(self.form_ns),
            pct(self.form_ns),
            ms(self.paint_ns),
            pct(self.paint_ns),
            ms(self.path_ns),
            pct(self.path_ns),
            ms(self.color_ns),
            pct(self.color_ns),
            self.text_ops,
            self.paint_ops,
            self.path_ops,
            self.image_ops,
            self.form_ops,
            self.color_ops,
            self.shading_rasters,
        )
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

    /// The buckets attribute *parts* of the total; the remainder is tokenizing
    /// and the operators not worth a clock read. Assert the documented direction
    /// (sum ≤ total) rather than an equality that is false by construction.
    #[test]
    fn work_buckets_are_attribution_not_a_partition() {
        let stats = InterpretStats {
            total_ns: 10_000,
            path_ns: 500,
            paint_ns: 700,
            text_ns: 1_200,
            image_ns: 2_000,
            form_ns: 3_000,
            color_ns: 100,
            ..Default::default()
        };
        let attributed = stats.path_ns
            + stats.paint_ns
            + stats.text_ns
            + stats.image_ns
            + stats.form_ns
            + stats.color_ns;
        assert!(
            attributed <= stats.total_ns,
            "attribution {attributed} must not exceed the total {}",
            stats.total_ns
        );
    }

    /// `describe` must report every bucket a reader needs, and must not divide
    /// by a zero total.
    #[test]
    fn describe_names_every_bucket_and_survives_a_zero_total() {
        let empty = InterpretStats::default().describe();
        for name in [
            "total", "shading", "text", "image", "form", "paint", "path", "color",
        ] {
            assert!(empty.contains(name), "missing {name} in: {empty}");
        }

        let stats = InterpretStats {
            total_ns: 1_000_000,
            image_ns: 600_000,
            form_ops: 7,
            ..Default::default()
        };
        let described = stats.describe();
        assert!(described.contains("image=0.60 60%"), "{described}");
        assert!(described.contains("form=7"), "{described}");
    }
}
