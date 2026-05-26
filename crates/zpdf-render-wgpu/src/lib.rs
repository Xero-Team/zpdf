use zpdf_display_list::RenderCommand;
use zpdf_render::{PageRenderInfo, RenderBackend};

pub struct WgpuRenderer {
    // Will hold: device, queue, pipelines, atlas, etc.
}

#[derive(Debug, thiserror::Error)]
pub enum WgpuRenderError {
    #[error("wgpu device not initialized")]
    NotInitialized,

    #[error("wgpu error: {0}")]
    Wgpu(String),
}

pub struct GpuTexture {
    pub width: u32,
    pub height: u32,
    // Will hold wgpu::Texture handle
}

impl RenderBackend for WgpuRenderer {
    type Target = GpuTexture;
    type Error = WgpuRenderError;

    fn begin_page(&mut self, _info: &PageRenderInfo) -> Result<(), Self::Error> {
        todo!("Phase 3: wgpu page setup")
    }

    fn execute(&mut self, _cmd: &RenderCommand) -> Result<(), Self::Error> {
        todo!("Phase 3: wgpu command execution")
    }

    fn end_page(&mut self) -> Result<Self::Target, Self::Error> {
        todo!("Phase 3: wgpu page finalization")
    }
}
