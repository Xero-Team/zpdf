pub mod dash;

use zpdf_core::Rect;
use zpdf_display_list::{Color, DisplayList, RenderCommand};

/// Render configuration for a page.
pub struct PageRenderInfo {
    pub page_rect: Rect,
    pub scale: f32,
    pub background: Color,
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
