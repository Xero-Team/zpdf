pub use zpdf_color::{IccCache, IccTransform};
pub use zpdf_content::interpreter::ContentInterpreter;
pub use zpdf_content::output_intent_cmyk_profile;
pub use zpdf_content::tables::{detect_tables, detect_tables_with_rules, RuleLine, Table};
pub use zpdf_content::text::{spans_to_text, struct_ordered_text, TextSpan};
pub use zpdf_core::*;
pub use zpdf_display_list as display_list;
pub use zpdf_display_list::DisplayList;
pub use zpdf_document::{
    AcroForm, Annotation, ByteRangeCoverage, CryptoStatus, DestView, Destination, DigestStatus,
    DocInfo, EmbeddedFile, EmbeddedSource, FieldKind, FieldValue, FormField,
    GeographicCoordinateSystem, Measure, OcConfig, OutlineItem, OutputIntent, PageLabelStyle,
    PageLabels, PdfDocument, PdfPage, ResourceDict, Signature, StructElem, StructKid, StructRole,
    StructTree, XmpMetadata,
};
pub use zpdf_font::FontCache;
pub use zpdf_image::{DecodedImage, ImageCache};
pub use zpdf_parser::PdfFile;
pub use zpdf_render::RenderBackend;

#[cfg(feature = "cpu-render")]
pub mod cpu {
    pub use zpdf_render_cpu::*;
}

#[cfg(feature = "gpu-render")]
pub mod gpu {
    pub use zpdf_render_wgpu::*;
}
