//! Prepare embedded Unicode text before mutating the destination object graph.
use crate::{invalid_data, DocumentBuilder, IncrementalWriter, ObjectIdMap};
use std::{collections::HashMap, sync::Arc};
use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Rect, Result};
use zpdf_document::{FieldKind, FormField, PdfDocument};
use zpdf_font::{system::SubstituteHints, FontCharacter, FontProvider, FontProviderLimits};

pub(crate) fn requires_unicode(text: &str) -> bool {
    text.chars().any(|ch| {
        !matches!(ch, '\n' | '\r' | '\t') && zpdf_document::unicode_to_winansi(ch).is_none()
    })
}
pub(crate) struct PreparedForm {
    doc: PdfDocument,
    pub bbox: Rect,
}
impl PreparedForm {
    pub fn object_budget(&self) -> usize {
        self.doc.file().all_object_ids().len() + 1
    }
    fn new(builder: DocumentBuilder, bbox: Rect) -> Result<Self> {
        Ok(Self {
            doc: PdfDocument::open(builder.build()?)?,
            bbox,
        })
    }
}
impl IncrementalWriter {
    fn text_fonts(&mut self) -> Arc<FontProvider> {
        if let Some(provider) = self.doc.font_provider() {
            return provider.clone();
        }
        let provider = Arc::new(FontProvider::new(true, FontProviderLimits::default()));
        self.set_font_provider(provider.clone());
        provider
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_unicode_text(
        &mut self,
        text: &str,
        font: &str,
        x: f64,
        y: f64,
        size: f64,
        color: (f64, f64, f64),
        bbox: Rect,
    ) -> Result<PreparedForm> {
        let provider = self.text_fonts();
        let hints = style_hints(font);
        ensure_coverage(&provider, text, font, hints)?;
        let mut builder = DocumentBuilder::new();
        let page = builder.add_page(bbox.width(), bbox.height());
        builder.add_text_with_fonts(page, text, x, y, &provider, hints, size, color)?;
        PreparedForm::new(builder, bbox)
    }

    pub(crate) fn prepare_unicode_field(
        &mut self,
        field: &FormField,
        rect: Rect,
        preferred_font: Option<&str>,
    ) -> Result<Option<PreparedForm>> {
        let Some(text) = field.display_value() else {
            return Ok(None);
        };
        let rect = rect.normalize();
        let (w, h) = (rect.width(), rect.height());
        if field.is_password() || w <= 1.0 || h <= 1.0 {
            return Ok(None);
        }
        let da = zpdf_document::forms::parse_da(
            field
                .default_appearance
                .as_deref()
                .unwrap_or("/Helv 0 Tf 0 g"),
        );
        let font = preferred_font.or(da.font.as_deref()).unwrap_or("Helvetica");
        let provider = self.text_fonts();
        let hints = style_hints(font);
        let text = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', " ");
        ensure_coverage(&provider, &text, font, hints)?;
        let mut metrics = HashMap::new();
        let mut ascent = 0.0f64;
        let mut descent = 0.0f64;
        for ch in text.chars().filter(|ch| *ch != '\n') {
            if metrics.contains_key(&ch) {
                continue;
            }
            let glyph = provider
                .resolve_character(
                    FontCharacter::from_text(&ch.to_string())
                        .ok_or_else(|| invalid_data("unsupported Unicode sequence"))?,
                    hints,
                )
                .ok_or_else(|| invalid_data("missing font coverage"))?;
            let face = ttf_parser::Face::parse(&glyph.font.data, glyph.font.face_index)
                .map_err(|_| invalid_data("invalid Unicode font"))?;
            let em = face.units_per_em() as f64;
            let advance = face
                .glyph_hor_advance(ttf_parser::GlyphId(glyph.glyph_id))
                .unwrap_or(face.units_per_em()) as f64;
            metrics.insert(ch, (advance / em * 1000.0).round() / 1000.0);
            ascent = ascent.max(face.ascender() as f64 / em);
            descent = descent.min(face.descender() as f64 / em);
        }
        let width = |s: &str, size: f64| {
            s.chars()
                .map(|ch| metrics.get(&ch).copied().unwrap_or(0.35) * size)
                .sum::<f64>()
        };
        let line_height = (ascent - descent).max(1.0);
        let mut size = if da.size > 0.0 {
            da.size
        } else {
            12.0f64.min((h - 4.0).max(1.0) / line_height)
        };
        let multiline = field.is_multiline()
            || (field.kind == FieldKind::Choice
                && field.flags & zpdf_document::forms::FF_COMBO == 0);
        let text = if multiline {
            text
        } else {
            text.replace('\n', " ")
        };
        let mut builder = DocumentBuilder::new();
        let page = builder.add_page(w, h);
        let color = da_color(&da.color_ops);
        if field.is_comb() {
            let count = field.max_len.unwrap_or(1).max(1) as usize;
            let cell = w / count as f64;
            if da.size == 0.0 {
                size = size.min(cell / metrics.values().copied().fold(1.0f64, f64::max));
            }
            let baseline = (h - (ascent + descent) * size) / 2.0;
            for (i, ch) in text.chars().take(count).enumerate() {
                let s = ch.to_string();
                let x = i as f64 * cell + (cell - width(&s, size)) / 2.0;
                builder
                    .add_text_with_fonts(page, &s, x, baseline, &provider, hints, size, color)?;
            }
        } else {
            if !multiline && da.size == 0.0 {
                let measured = width(&text, size);
                if measured > (w - 4.0).max(1.0) {
                    size *= (w - 4.0).max(1.0) / measured;
                }
            }
            let wrap = |size: f64| -> Vec<String> {
                if !multiline {
                    return vec![text.clone()];
                }
                let mut lines = Vec::new();
                let mut current = String::new();
                let mut used = 0.0;
                for ch in text.chars() {
                    let advance = metrics.get(&ch).copied().unwrap_or(0.35) * size;
                    if ch == '\n' || (!current.is_empty() && used + advance > (w - 4.0).max(1.0)) {
                        lines.push(std::mem::take(&mut current));
                        used = 0.0;
                    }
                    if ch != '\n' {
                        current.push(ch);
                        used += advance;
                    }
                }
                lines.push(current);
                lines
            };
            let mut lines = wrap(size);
            if multiline && da.size == 0.0 {
                while size > 2.0 && lines.len() as f64 * size * line_height > (h - 4.0).max(1.0) {
                    size = (size - 0.5).max(2.0);
                    lines = wrap(size);
                }
            }
            let mut y = if multiline {
                h - 2.0 - ascent * size
            } else {
                (h - (ascent + descent) * size) / 2.0
            };
            for line in lines {
                if y + ascent * size < 0.0 {
                    break;
                }
                let measured = width(&line, size);
                let x = match field.quadding {
                    1 => (w - measured) / 2.0,
                    2 => w - 2.0 - measured,
                    _ => 2.0,
                };
                builder.add_text_with_fonts(page, &line, x, y, &provider, hints, size, color)?;
                y -= size * line_height;
            }
        }
        Ok(Some(PreparedForm::new(builder, Rect::new(0.0, 0.0, w, h))?))
    }

    pub(crate) fn import_text_form(&mut self, prepared: &PreparedForm) -> Result<ObjectId> {
        self.ensure_object_capacity(prepared.object_budget())?;
        let page = prepared.doc.page(0)?;
        let page_obj = prepared.doc.file().resolve(page.id)?;
        let resources = page_obj
            .as_dict()?
            .get("Resources")
            .cloned()
            .unwrap_or(PdfObject::Dict(PdfDict::new()));
        let mut map = ObjectIdMap::new();
        let resources =
            crate::copier::remap_via_copy(&resources, prepared.doc.file(), self, &mut map, &[])?;
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName::new("Type"),
            PdfObject::Name(PdfName::new("XObject")),
        );
        dict.insert(
            PdfName::new("Subtype"),
            PdfObject::Name(PdfName::new("Form")),
        );
        let b = prepared.bbox;
        dict.insert(
            PdfName::new("BBox"),
            PdfObject::Array(
                [b.x0, b.y0, b.x1, b.y1]
                    .into_iter()
                    .map(PdfObject::Real)
                    .collect(),
            ),
        );
        dict.insert(PdfName::new("Resources"), resources);
        let bytes = prepared.doc.page_content_bytes(&page)?;
        let (n, g) = self.try_add_flate_stream(&dict, &bytes)?;
        Ok(ObjectId(n, g as u16))
    }
}

fn style_hints(font: &str) -> SubstituteHints {
    let font = font.to_ascii_lowercase();
    SubstituteHints {
        bold: font.contains("bold"),
        italic: font.contains("italic") || font.contains("oblique"),
        ..Default::default()
    }
}
fn ensure_coverage(
    provider: &FontProvider,
    text: &str,
    font: &str,
    hints: SubstituteHints,
) -> Result<()> {
    if text.chars().count() > 16_384 {
        return Err(invalid_data("Unicode appearance exceeds 16384 characters").into());
    }
    let covered = || {
        text.chars()
            .filter(|ch| !matches!(ch, '\n' | '\r' | '\t'))
            .all(|ch| {
                FontCharacter::from_text(&ch.to_string())
                    .and_then(|c| provider.resolve_character(c, hints))
                    .is_some()
            })
    };
    for (name, ordering) in [
        (font, None),
        ("ZpdfUnicodeFallback", Some("GB1")),
        ("ZpdfUnicodeFallback", Some("CNS1")),
        ("ZpdfUnicodeFallback", Some("Japan1")),
        ("ZpdfUnicodeFallback", Some("Korea1")),
        ("Arial Unicode MS", None),
        ("Segoe UI Symbol", None),
    ] {
        if covered() {
            return Ok(());
        }
        if let Some(face) = provider.find_font(name, hints, ordering) {
            provider.register(face.data, face.face_index)?;
        }
    }
    if covered() {
        Ok(())
    } else {
        Err(invalid_data(
            "no font covers the requested text; register a font provider or use --font-file",
        )
        .into())
    }
}
fn da_color(ops: &str) -> (f64, f64, f64) {
    let tokens: Vec<_> = ops.split_whitespace().collect();
    let n: Vec<f64> = tokens.iter().filter_map(|v| v.parse().ok()).collect();
    match (tokens.last().copied(), n.as_slice()) {
        (Some("g"), [g]) => (*g, *g, *g),
        (Some("rg"), [r, g, b]) => (*r, *g, *b),
        (Some("k"), [c, m, y, k]) => zpdf_color::cmyk_to_rgb(*c, *m, *y, *k),
        _ => (0.0, 0.0, 0.0),
    }
}
