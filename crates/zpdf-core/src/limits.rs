/// Security limits for PDF parsing. PDF is an untrusted input format.
#[derive(Debug, Clone)]
pub struct ParseLimits {
    pub max_object_depth: u32,
    pub max_stream_bytes: u64,
    pub max_image_pixels: u64,
    pub max_page_operators: u64,
    pub max_string_length: u32,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_object_depth: 100,
            max_stream_bytes: 256 * 1024 * 1024,
            max_image_pixels: 100_000_000,
            max_page_operators: 1_000_000,
            max_string_length: 65536,
        }
    }
}
