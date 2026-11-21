use crate::{Error, Result};

/// Bounded, machine-readable evidence that processing returned a partial result.
/// Counters saturate and contain no input-controlled strings. A clean report
/// means no known loss was encountered, not a claim of full PDF conformance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessingDiagnostics {
    pub resource_limit_reached: bool,
    pub time_limit_reached: bool,
    pub decode_failures: u32,
    pub missing_resources: u32,
    pub unsupported_features: u32,
    pub malformed_content: u32,
    /// Embedded font programs that failed to decode or parse and fell back to a
    /// substitute (or metrics-only fallback). Normal non-embedded fonts are not counted.
    pub embedded_font_fallbacks: u32,
    /// Embedded font fallbacks whose shown visible glyphs could not all be
    /// drawn (missing substitute, partial coverage, or unmappable PUA/CID glyphs).
    /// Visual exports treat this as incomplete, while text-only exports still
    /// succeed when every shown character decoded cleanly.
    pub missing_glyph_outlines: u32,
    /// Zero-based first affected page, populated by high-level APIs.
    pub first_affected_page: Option<usize>,
    /// First failed indirect object, when the failing operation has one.
    pub first_failed_object: Option<crate::ObjectId>,
}
impl ProcessingDiagnostics {
    /// Whether processing produced complete visual output without known missing content.
    ///
    /// A recovered embedded font fallback (`embedded_font_fallbacks > 0` with
    /// all failure and omission counters zero) still leaves `is_complete()` true
    /// while preserving the fallback diagnostic. When a fallback cannot draw
    /// every shown visible glyph, `missing_glyph_outlines` (and `decode_failures`
    /// if text decoding also failed) is incremented so `is_complete()` becomes
    /// false and [`ensure_complete`](Self::ensure_complete) rejects the incomplete
    /// output.
    pub fn is_complete(&self) -> bool {
        self.is_text_complete() && self.missing_glyph_outlines == 0
    }
    /// Whether text extraction completed without known omissions or limits.
    ///
    /// Missing glyph outlines from an unrecovered embedded font fallback do not
    /// fail text-only checks when every shown character still decoded cleanly.
    pub fn is_text_complete(&self) -> bool {
        !self.resource_limit_reached
            && !self.time_limit_reached
            && self.decode_failures == 0
            && self.missing_resources == 0
            && self.unsupported_features == 0
            && self.malformed_content == 0
    }
    /// Whether no failure, limit, or embedded-font fallback has been recorded.
    pub fn is_empty(&self) -> bool {
        self.is_complete() && self.embedded_font_fallbacks == 0
    }
    pub fn ensure_complete(&self) -> Result<()> {
        if self.is_complete() {
            Ok(())
        } else {
            Err(Error::IncompleteOutput(*self))
        }
    }
    /// Reject partial text output while allowing decoded text whose only
    /// omission is missing fallback glyph outlines.
    pub fn ensure_text_complete(&self) -> Result<()> {
        if self.is_text_complete() {
            Ok(())
        } else {
            Err(Error::IncompleteOutput(*self))
        }
    }
    pub fn merge(&mut self, other: Self) {
        self.first_affected_page = self.first_affected_page.or(other.first_affected_page);
        self.first_failed_object = self.first_failed_object.or(other.first_failed_object);
        self.resource_limit_reached |= other.resource_limit_reached;
        self.time_limit_reached |= other.time_limit_reached;
        self.decode_failures = self.decode_failures.saturating_add(other.decode_failures);
        self.missing_resources = self
            .missing_resources
            .saturating_add(other.missing_resources);
        self.unsupported_features = self
            .unsupported_features
            .saturating_add(other.unsupported_features);
        self.malformed_content = self
            .malformed_content
            .saturating_add(other.malformed_content);
        self.embedded_font_fallbacks = self
            .embedded_font_fallbacks
            .saturating_add(other.embedded_font_fallbacks);
        self.missing_glyph_outlines = self
            .missing_glyph_outlines
            .saturating_add(other.missing_glyph_outlines);
    }
    pub fn record_decode_failure(&mut self, error: &Error) {
        self.decode_failures = self.decode_failures.saturating_add(1);
        self.resource_limit_reached |= matches!(
            error,
            Error::RecursionLimit(_) | Error::StreamSizeLimit(_) | Error::StringLengthLimit(_)
        );
        if let Error::ObjectNotFound(id) = error {
            self.first_failed_object.get_or_insert(*id);
        }
    }
    pub fn record_object_failure(&mut self, id: crate::ObjectId, error: &Error) {
        self.record_decode_failure(error);
        self.first_failed_object.get_or_insert(id);
    }
    pub fn record_embedded_font_fallback(&mut self, id: Option<crate::ObjectId>) {
        self.embedded_font_fallbacks = self.embedded_font_fallbacks.saturating_add(1);
        if let Some(id) = id {
            self.first_failed_object.get_or_insert(id);
        }
    }
    pub fn record_missing_glyph_outline(&mut self, id: Option<crate::ObjectId>) {
        self.missing_glyph_outlines = self.missing_glyph_outlines.saturating_add(1);
        if let Some(id) = id {
            self.first_failed_object.get_or_insert(id);
        }
    }
    pub fn set_page(&mut self, index: usize) {
        if !self.is_empty() {
            self.first_affected_page.get_or_insert(index);
        }
    }
    /// Serialize the fixed diagnostic schema without a serialization dependency.
    /// Object IDs are `[number, generation]`; absent locations are JSON null.
    pub fn to_json(&self) -> String {
        let page = self
            .first_affected_page
            .map_or_else(|| "null".into(), |n| n.to_string());
        let object = self
            .first_failed_object
            .map_or_else(|| "null".into(), |id| format!("[{},{}]", id.0, id.1));
        format!("{{\"complete\":{},\"resource_limit_reached\":{},\"time_limit_reached\":{},\"decode_failures\":{},\"missing_resources\":{},\"unsupported_features\":{},\"malformed_content\":{},\"embedded_font_fallbacks\":{},\"missing_glyph_outlines\":{},\"first_affected_page\":{page},\"first_failed_object\":{object}}}",
            self.is_complete(), self.resource_limit_reached, self.time_limit_reached, self.decode_failures,
            self.missing_resources, self.unsupported_features, self.malformed_content, self.embedded_font_fallbacks, self.missing_glyph_outlines)
    }
}
impl std::fmt::Display for ProcessingDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resource_limit={} time_limit={} decode_failures={} missing_resources={} unsupported_features={} malformed_content={} embedded_font_fallbacks={} missing_glyph_outlines={} first_page={:?} first_object={:?}",
            self.resource_limit_reached, self.time_limit_reached, self.decode_failures,
            self.missing_resources, self.unsupported_features, self.malformed_content,
            self.embedded_font_fallbacks, self.missing_glyph_outlines, self.first_affected_page, self.first_failed_object)
    }
}
