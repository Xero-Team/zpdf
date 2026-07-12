/// Security limits for PDF parsing. PDF is an untrusted input format.
#[derive(Debug, Clone)]
pub struct ParseLimits {
    pub max_object_depth: u32,
    pub max_stream_bytes: u64,
    pub max_image_pixels: u64,
    pub max_page_operators: u64,
    pub max_string_length: u32,
    /// Max number of objects a tail-scan recovery pass will index before
    /// giving up (guards against pathological/adversarial inputs).
    pub max_objects: u32,

    // ─── New Cumulative Budget Fields (2026-07-10 Security Audit) ───
    /// Maximum total bytes for decoded filter output (cumulative across filter chain).
    /// Prevents decompression bombs where small input expands to GiB output.
    /// Default: same as max_stream_bytes (256 MiB).
    pub max_decoded_stream_bytes: u64,

    /// Maximum entries in operand stack during content stream interpretation.
    /// Prevents operand-only streams from consuming unlimited heap.
    /// Default: 10,000 operands.
    pub max_operand_stack_depth: u32,

    /// Maximum graphics state stack depth (q/Q operators).
    /// Each entry clones a large GraphicsState; unbounded nesting causes multi-GiB memory.
    /// Default: 256 levels.
    pub max_graphics_state_depth: u32,

    /// Maximum marked-content nesting depth (BMC/BDC/EMC operators).
    /// Default: 128 levels.
    pub max_marked_content_depth: u32,

    /// Maximum blend/transparency group nesting depth.
    /// Each level allocates full-page offscreen surfaces.
    /// Default: 16 levels.
    pub max_blend_group_depth: u32,

    /// Maximum total bytes for object cache (parsed PDF objects).
    /// Default: 512 MiB.
    pub max_object_cache_bytes: u64,

    /// Maximum total bytes for decoded object streams.
    /// Default: 256 MiB.
    pub max_objstm_cache_bytes: u64,

    /// Maximum total bytes for decoded images (RGBA).
    /// Default: 1 GiB (allows ~2-3 large 100MP images simultaneously).
    pub max_image_cache_bytes: u64,

    /// Maximum total bytes for parsed font data (programs + tables).
    /// Default: 256 MiB.
    pub max_font_cache_bytes: u64,

    /// Maximum total bytes for CPU soft-mask coverage planes.
    /// Default: 512 MiB (~8 full-page masks at 64MP).
    pub max_softmask_cache_bytes: u64,

    /// Maximum total pixels for a single rendered page (CPU or GPU).
    /// Enforced at rasterization time to prevent multi-GiB framebuffer allocations.
    /// Default: 64,000,000 pixels (~8K×8K or 25K×2.5K).
    pub max_page_pixels: u64,

    /// Maximum total bytes for GPU textures uploaded per page.
    /// Default: 1 GiB.
    pub max_gpu_texture_bytes: u64,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_object_depth: 100,
            max_stream_bytes: 256 * 1024 * 1024,
            max_image_pixels: 100_000_000,
            max_page_operators: 1_000_000,
            // ISO 32000 imposes no string length limit (64 KiB is only the
            // PDF/A-1 / legacy-Acrobat bound), so real files exceed it. This
            // is purely an allocation guard against adversarial input.
            max_string_length: 16 * 1024 * 1024,
            max_objects: 5_000_000,

            // New cumulative budget defaults
            max_decoded_stream_bytes: 256 * 1024 * 1024, // Match max_stream_bytes
            max_operand_stack_depth: 10_000,
            max_graphics_state_depth: 256,
            max_marked_content_depth: 128,
            max_blend_group_depth: 16,
            max_object_cache_bytes: 512 * 1024 * 1024,
            max_objstm_cache_bytes: 256 * 1024 * 1024,
            max_image_cache_bytes: 1024 * 1024 * 1024,
            max_font_cache_bytes: 256 * 1024 * 1024,
            max_softmask_cache_bytes: 512 * 1024 * 1024,
            max_page_pixels: 64_000_000,
            max_gpu_texture_bytes: 1024 * 1024 * 1024,
        }
    }
}
