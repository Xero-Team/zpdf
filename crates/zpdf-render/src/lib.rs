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
}
