//! Selecting and compiling the document's active output-intent DeviceCMYK
//! profile for the render pipeline.
//!
//! The document model (`zpdf-document`) parses `/OutputIntents` into
//! [`OutputIntent`] metadata but stays free of colour-management code. This
//! module bridges that metadata to the colour pipeline: it picks the effective
//! intent for a page and compiles its `/DestOutputProfile` into an
//! [`IccTransform`], which the render driver hands to
//! [`ContentInterpreter::with_output_intent_cmyk`](crate::interpreter::ContentInterpreter::with_output_intent_cmyk).

use std::sync::Arc;

use zpdf_color::{IccCache, IccTransform, RenderIntent};
use zpdf_document::OutputIntent;
use zpdf_parser::PdfFile;

/// Compile the DeviceCMYK colour-management transform for a page's active
/// output intent, or `None` when none applies.
///
/// Page-level intents (PDF 2.0) take precedence: the page's intents are searched
/// for a CMYK profile first, and only if the page declares none does the search
/// fall back to the document-level intents. (So a page that declares only a
/// non-CMYK intent — e.g. an RGB or PDF/E condition — does not suppress a
/// governing document-level CMYK intent.) Within a level, the first intent whose
/// `/DestOutputProfile` compiles to a 4-channel (CMYK) ICC profile wins.
///
/// `None` — meaning the renderer keeps the Adobe SWOP polynomial — is returned
/// when there is no output intent, no embedded profile, or no profile is
/// 4-channel / they fail to parse, so a document without a usable CMYK intent
/// renders exactly as it did before.
///
/// The profile is compiled through `cache` (shared with the page's other ICC
/// work, and remembering failures) under the media-relative-colorimetric
/// default intent: an output intent characterizes an output device, so it is
/// fixed at document scope rather than re-resolved per `ri` operator.
pub fn output_intent_cmyk_profile(
    file: &PdfFile,
    page_intents: &[OutputIntent],
    doc_intents: &[OutputIntent],
    cache: &mut IccCache,
) -> Option<Arc<IccTransform>> {
    if let Some(t) = select_cmyk(file, page_intents, cache) {
        return Some(t);
    }
    select_cmyk(file, doc_intents, cache)
}

/// The first intent in `intents` whose `/DestOutputProfile` compiles to a
/// 4-channel (CMYK) ICC transform.
///
/// The embedded ICC profile's data colour space is authoritative — a profile is
/// accepted purely on its compiled `components() == 4`, regardless of a
/// possibly-mistyped `/N` in the stream dictionary (reference renderers key off
/// the ICC header too). Every present `/DestOutputProfile` is therefore
/// compiled (results, including failures, are cached) rather than pre-filtered
/// on `/N`.
fn select_cmyk(
    file: &PdfFile,
    intents: &[OutputIntent],
    cache: &mut IccCache,
) -> Option<Arc<IccTransform>> {
    let intent = RenderIntent::default();
    for oi in intents {
        let Some(id) = oi.dest_output_profile else {
            continue;
        };
        if let Some(t) = cache.get_or_build(id, intent, || file.resolve_stream_data(id).ok()) {
            if t.components() == 4 {
                return Some(t);
            }
        }
    }
    None
}
