//! Multilingual scalar authoring through the same coverage provider as rendering.

use super::{DocumentBuilder, EmbeddedFontHandle, PageHandle};
use zpdf_core::{Error, Result};
use zpdf_font::system::SubstituteHints;
use zpdf_font::{FontCharacter, FontProvider};

fn invalid(message: impl Into<String>) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message.into(),
    ))
}

impl DocumentBuilder {
    /// Embed one TTC face as an independent sfnt before ordinary subsetting.
    /// Face indices are validated; table offsets and checksums are rebuilt.
    pub fn embed_composite_font_face(
        &mut self,
        bytes: &[u8],
        face_index: u32,
    ) -> Result<EmbeddedFontHandle> {
        self.embed_composite_font(standalone_face(bytes, face_index)?)
    }

    /// Write horizontal multilingual text with registered coverage selection.
    /// Each chosen face is embedded once and subset at build time. Placement
    /// uses the same rounded 1/1000-em advances written to the PDF width array.
    ///
    /// This scalar authoring API does not perform shaping, bidi reordering or
    /// variation-sequence layout. Unsupported scalars/selectors return an error
    /// before adding page items; existing content is not partially modified.
    #[allow(clippy::too_many_arguments)]
    pub fn add_text_with_fonts(
        &mut self,
        page: PageHandle,
        text: &str,
        x: f64,
        y: f64,
        provider: &FontProvider,
        hints: SubstituteHints,
        size: f64,
        color: (f64, f64, f64),
    ) -> Result<()> {
        if page.0 as usize >= self.pages.len()
            || ![x, y, size, color.0, color.1, color.2]
                .into_iter()
                .all(f64::is_finite)
            || size <= 0.0
        {
            return Err(invalid("invalid page, position, color or font size"));
        }
        let mut selected = Vec::new();
        for ch in text.chars() {
            let mut encoded = [0; 4];
            let character =
                FontCharacter::from_text(ch.encode_utf8(&mut encoded)).ok_or_else(|| {
                    invalid("variation selectors require a positioned-glyph authoring API")
                })?;
            let glyph = provider
                .resolve_character(character, hints)
                .ok_or_else(|| invalid(format!("no registered font covers U+{:04X}", ch as u32)))?;
            selected.push((ch, glyph));
        }
        // Complete coverage validation before computing metrics or embedding;
        // an unsupported suffix needs none of the prefix's layout work.
        let start_fonts = self.embedded_fonts.len();
        let start_faces = self.provider_faces.len();
        let result = (|| {
            let mut runs: Vec<(EmbeddedFontHandle, String, f64)> = Vec::new();
            let mut pen = x;
            // Parse each selected face once, not once per character.
            let mut faces: Vec<(*const u8, u32, ttf_parser::Face<'_>)> = Vec::new();
            for (ch, glyph) in &selected {
                let (ch, key) = (*ch, (glyph.font.data.as_ptr(), glyph.font.face_index));
                let index = match faces
                    .iter()
                    .position(|(data, index, _)| (*data, *index) == key)
                {
                    Some(index) => index,
                    None => {
                        let face = ttf_parser::Face::parse(&glyph.font.data, key.1)
                            .map_err(|error| invalid(format!("invalid selected font: {error}")))?;
                        faces.push((key.0, key.1, face));
                        faces.len() - 1
                    }
                };
                let face = &faces[index].2;
                let width = face
                    .glyph_hor_advance(ttf_parser::GlyphId(glyph.glyph_id))
                    .map(|width| {
                        (width as f64 * 1000.0 / face.units_per_em() as f64)
                            .round()
                            .max(0.0) as u16
                    })
                    .unwrap_or(1000);
                let position = pen;
                pen += width as f64 / 1000.0 * size;
                if !pen.is_finite() {
                    return Err(invalid("text position overflow"));
                }
                let handle = if let Some((_, _, handle)) =
                    self.provider_faces.iter().find(|(bytes, index, _)| {
                        *index == glyph.font.face_index
                            && (std::sync::Arc::ptr_eq(bytes, &glyph.font.data)
                                || bytes.as_ref() == glyph.font.data.as_ref())
                    }) {
                    *handle
                } else {
                    let handle =
                        self.embed_composite_font_face(&glyph.font.data, glyph.font.face_index)?;
                    self.provider_faces.push((
                        std::sync::Arc::clone(&glyph.font.data),
                        glyph.font.face_index,
                        handle,
                    ));
                    handle
                };
                if let Some((_, text, _)) = runs.last_mut().filter(|(font, _, _)| *font == handle) {
                    text.push(ch);
                } else {
                    runs.push((handle, ch.to_string(), position));
                }
            }
            Ok(runs)
        })();
        let runs = match result {
            Ok(runs) => runs,
            Err(error) => {
                self.embedded_fonts.truncate(start_fonts);
                self.provider_faces.truncate(start_faces);
                return Err(error);
            }
        };
        for (font, text, x) in runs {
            self.add_text_embedded(page, &text, x, y, font, size, color)?;
        }
        Ok(())
    }
}

fn standalone_face(bytes: &[u8], face_index: u32) -> Result<Vec<u8>> {
    let face = ttf_parser::Face::parse(bytes, face_index)
        .map_err(|error| invalid(format!("invalid font face: {error}")))?;
    if !bytes.starts_with(b"ttcf") {
        return Ok(bytes.to_vec());
    }
    let raw = face.raw_face();
    if raw.table_records.is_empty() || raw.table_records.len() > 4095 {
        return Err(invalid("invalid TTC table count"));
    }
    // Validate before copying. Overlapping records must not amplify a small
    // input into gigabytes of repeated table payloads.
    let mut total = 0usize;
    let mut tags = std::collections::HashSet::new();
    for record in raw.table_records {
        if !tags.insert(record.tag) {
            return Err(invalid("duplicate TTC table tag"));
        }
        total = total
            .checked_add(record.length as usize)
            .filter(|&size| size <= bytes.len())
            .ok_or_else(|| invalid("TTC table payload exceeds input size"))?;
    }
    let mut tables = Vec::new();
    for record in raw.table_records {
        let tag = record.tag.to_bytes();
        if &tag == b"DSIG" {
            continue;
        }
        let data = raw
            .table(record.tag)
            .ok_or_else(|| invalid("invalid TTC table range"))?;
        let mut data = data.to_vec();
        if &tag == b"head" {
            data.get_mut(8..12)
                .ok_or_else(|| invalid("truncated TTC head"))?
                .fill(0);
        }
        tables.push((tag, data));
    }
    tables.sort_unstable_by_key(|(tag, _)| *tag);
    if tables.is_empty() || tables.len() > 4095 {
        return Err(invalid("invalid TTC table count"));
    }
    let count = tables.len() as u16;
    let power = 1u16 << (15 - count.leading_zeros());
    let mut output = Vec::new();
    output.extend_from_slice(
        if face.tables().cff.is_some() || face.tables().cff2.is_some() {
            b"OTTO"
        } else {
            &[0, 1, 0, 0]
        },
    );
    output.extend_from_slice(&count.to_be_bytes());
    output.extend_from_slice(&(power * 16).to_be_bytes());
    output.extend_from_slice(&(power.trailing_zeros() as u16).to_be_bytes());
    output.extend_from_slice(&(count * 16 - power * 16).to_be_bytes());
    let mut offset = 12usize + tables.len() * 16;
    let mut head = None;
    for (tag, data) in &tables {
        output.extend_from_slice(tag);
        output.extend_from_slice(&checksum(data).to_be_bytes());
        output.extend_from_slice(
            &u32::try_from(offset)
                .map_err(|_| invalid("TTC output too large"))?
                .to_be_bytes(),
        );
        output.extend_from_slice(
            &u32::try_from(data.len())
                .map_err(|_| invalid("TTC table too large"))?
                .to_be_bytes(),
        );
        if tag == b"head" {
            head = Some(offset);
        }
        offset = offset
            .checked_add(data.len())
            .and_then(|value| value.checked_add(3))
            .map(|value| value & !3)
            .ok_or_else(|| invalid("TTC size overflow"))?;
    }
    for (_, data) in tables {
        output.extend_from_slice(&data);
        while output.len() % 4 != 0 {
            output.push(0);
        }
    }
    let head = head.ok_or_else(|| invalid("TTC has no head table"))?;
    let adjustment = 0xB1B0AFBAu32.wrapping_sub(checksum(&output));
    output[head + 8..head + 12].copy_from_slice(&adjustment.to_be_bytes());
    ttf_parser::Face::parse(&output, 0)
        .map_err(|error| invalid(format!("rebuilt TTC face is invalid: {error}")))?;
    Ok(output)
}

fn checksum(bytes: &[u8]) -> u32 {
    bytes.chunks(4).fold(0u32, |sum, chunk| {
        let mut word = [0; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}
