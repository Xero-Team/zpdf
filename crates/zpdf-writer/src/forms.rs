//! Form filling: set interactive-form field values and regenerate appearances.
//!
//! [`FormFiller`] rewrites field `/V` values and (for text/choice fields)
//! regenerates widget `/AP` appearance streams by reusing the existing
//! appearance engine from [`zpdf_document::forms`].

use tracing::warn;
use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Rect, Result};
use zpdf_document::{generate_widget_appearance, FieldKind, FieldValue, FormField, FF_READONLY};

use crate::metadata::encode_text_string;
use crate::{invalid_data, IncrementalWriter};

/// A form-filling session. Call [`FormFiller::set`] for each field to fill,
/// then [`FormFiller::finish`] to flush the `/NeedAppearances` flag if needed.
pub struct FormFiller<'w> {
    writer: &'w mut IncrementalWriter,
    /// Widget ids whose appearance could not be regenerated (font outside the
    /// standard-14, or no `/DA` at all) — triggers `/NeedAppearances true`.
    need_appearances: bool,
    dr_fonts: Option<PdfDict>,
}

impl<'w> FormFiller<'w> {
    /// Create a new form filler. Errors when the document has no AcroForm.
    pub fn new(writer: &'w mut IncrementalWriter) -> Result<Self> {
        let dr_fonts = writer
            .document()
            .acro_form()
            .and_then(|af| af.dr_fonts.clone());
        if writer.document().acro_form().is_none() {
            return Err(invalid_data("document has no AcroForm; cannot fill fields").into());
        }
        Ok(Self {
            writer,
            need_appearances: false,
            dr_fonts,
        })
    }

    /// Set a field's value by its fully-qualified name (the `/T` path joined
    /// by `.`). Errors when the field is not found or is a signature field.
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        let form = self
            .writer
            .document()
            .acro_form()
            .ok_or_else(|| invalid_data("no AcroForm"))?;
        let field = form
            .fields
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| invalid_data(&format!("field not found: {name}")))?
            .clone();

        if field.kind == FieldKind::Signature {
            return Err(invalid_data("cannot set value of signature fields").into());
        }
        if field.flags & FF_READONLY != 0 {
            warn!("field {name} has read-only flag; setting value anyway");
        }

        match field.kind {
            FieldKind::Text | FieldKind::Choice => self.set_text_choice(&field, value)?,
            FieldKind::Button => self.set_button(&field, value)?,
            _ => {}
        }
        Ok(())
    }

    /// Flush the `/NeedAppearances` flag if any widget could not be regenerated.
    pub fn finish(self) -> Result<()> {
        if !self.need_appearances {
            return Ok(());
        }
        let catalog_ref = self.writer.catalog_ref;
        let catalog = self.writer.resolve_current(catalog_ref)?;
        let catalog_dict = catalog.as_dict()?;
        let af_ref = catalog_dict
            .get("AcroForm")
            .ok_or_else(|| invalid_data("catalog has no /AcroForm"))?
            .clone();

        match af_ref {
            PdfObject::Ref(r) => {
                let mut af_dict = self.writer.resolve_current(r)?.as_dict()?.clone();
                af_dict.insert(PdfName::new("NeedAppearances"), PdfObject::Bool(true));
                self.writer.overwrite_object(r, PdfObject::Dict(af_dict));
            }
            PdfObject::Dict(mut d) => {
                d.insert(PdfName::new("NeedAppearances"), PdfObject::Bool(true));
                let mut new_catalog = catalog_dict.clone();
                new_catalog.insert(PdfName::new("AcroForm"), PdfObject::Dict(d));
                self.writer
                    .overwrite_object(catalog_ref, PdfObject::Dict(new_catalog));
            }
            _ => {}
        }
        Ok(())
    }

    fn set_text_choice(&mut self, field: &FormField, value: &str) -> Result<()> {
        // Prepare every update before mutating the writer. This lets object
        // exhaustion fail without changing the field or only some widgets.
        let mut field_dict = self
            .writer
            .resolve_current(field.field_id)?
            .as_dict()?
            .clone();
        field_dict.insert(
            PdfName::new("V"),
            PdfObject::String(encode_text_string(value)),
        );
        // Remove /I (selected indices) for choice fields.
        if field.kind == FieldKind::Choice {
            field_dict.0.remove(&PdfName::new("I"));
        }
        let mut filled = field.clone();
        filled.value = Some(FieldValue::Text(value.to_string()));
        let mut prepared_widgets = Vec::with_capacity(field.widgets.len());
        let mut appearance_count = 0usize;
        for &widget_id in &field.widgets {
            let widget_dict = self.writer.resolve_current(widget_id)?.as_dict()?.clone();
            let rect = rect_from_array(widget_dict.get("Rect"))?;
            if widget_will_have_appearance(field, value, rect) {
                appearance_count = appearance_count
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("too many widget appearances"))?;
            }
            prepared_widgets.push((widget_id, widget_dict, rect));
        }

        self.writer.ensure_object_capacity(appearance_count)?;
        let mut widget_updates = Vec::with_capacity(prepared_widgets.len());
        let mut need_appearances = false;
        for (widget_id, mut widget_dict, rect) in prepared_widgets {
            if let Some(ap) = generate_widget_appearance(&filled, rect, self.dr_fonts.as_ref()) {
                let mut ap_dict = PdfDict::new();
                ap_dict.insert(
                    PdfName::new("Type"),
                    PdfObject::Name(PdfName::new("XObject")),
                );
                ap_dict.insert(
                    PdfName::new("Subtype"),
                    PdfObject::Name(PdfName::new("Form")),
                );
                ap_dict.insert(PdfName::new("FormType"), PdfObject::Integer(1));
                ap_dict.insert(
                    PdfName::new("BBox"),
                    PdfObject::Array(vec![
                        PdfObject::Real(ap.bbox.x0),
                        PdfObject::Real(ap.bbox.y0),
                        PdfObject::Real(ap.bbox.x1),
                        PdfObject::Real(ap.bbox.y1),
                    ]),
                );
                ap_dict.insert(PdfName::new("Resources"), PdfObject::Dict(ap.resources));
                let ap_ref = self.writer.try_add_flate_stream(&ap_dict, &ap.content)?;
                let mut ap_outer = PdfDict::new();
                ap_outer.insert(
                    PdfName::new("N"),
                    PdfObject::Ref(ObjectId(ap_ref.0, ap_ref.1 as u16)),
                );
                widget_dict.insert(PdfName::new("AP"), PdfObject::Dict(ap_outer));
            } else {
                need_appearances = true;
                widget_dict.0.remove(&PdfName::new("AP"));
            }
            widget_updates.push((widget_id, widget_dict));
        }

        self.writer
            .overwrite_object(field.field_id, PdfObject::Dict(field_dict));
        for (widget_id, widget_dict) in widget_updates {
            self.writer
                .overwrite_object(widget_id, PdfObject::Dict(widget_dict));
        }
        self.need_appearances |= need_appearances;
        Ok(())
    }

    fn set_button(&mut self, field: &FormField, value: &str) -> Result<()> {
        // Map value string to a state name.
        let normalized = value.to_lowercase();
        let state_name = match normalized.as_str() {
            "off" | "false" | "" => "Off".to_string(),
            "yes" | "true" | "on" => {
                // When unambiguous (one on-state across all widgets), use it.
                let on_states = self.collect_button_on_states(field)?;
                if on_states.len() == 1 {
                    on_states.into_iter().next().unwrap()
                } else {
                    return Err(invalid_data(&format!(
                        "ambiguous 'on' value for button field {}; specify exact state name",
                        field.name
                    ))
                    .into());
                }
            }
            _ => {
                // Exact on-state name, or for radio with /Opt, an option string.
                let on_states = self.collect_button_on_states(field)?;
                if on_states.iter().any(|state| state == value) {
                    value.to_string()
                } else if !field.options.is_empty() {
                    // Radio with /Opt: map option export value to widget index.
                    let idx = field
                        .options
                        .iter()
                        .position(|(exp, _)| exp == value)
                        .ok_or_else(|| invalid_data(&format!("option not found: {value}")))?;
                    if idx < field.widgets.len() {
                        let w = field.widgets[idx];
                        let wdict = self.writer.resolve_current(w)?.as_dict()?.clone();
                        let wstates = on_states_from_widget(&wdict, self.writer)?;
                        wstates
                            .into_iter()
                            .next()
                            .unwrap_or_else(|| "Yes".to_string())
                    } else {
                        return Err(invalid_data("option index out of widget range").into());
                    }
                } else {
                    value.to_string()
                }
            }
        };

        // Write /V on the field dict.
        let mut field_dict = self
            .writer
            .resolve_current(field.field_id)?
            .as_dict()?
            .clone();
        field_dict.insert(
            PdfName::new("V"),
            PdfObject::Name(PdfName::new(&state_name)),
        );
        self.writer
            .overwrite_object(field.field_id, PdfObject::Dict(field_dict));

        // Set /AS on each widget: the state if the widget has it, else /Off.
        for &widget_id in &field.widgets {
            let widget_dict = self.writer.resolve_current(widget_id)?.as_dict()?.clone();
            let states = on_states_from_widget(&widget_dict, self.writer)?;
            let as_state = if states.contains(&state_name) {
                &state_name
            } else {
                "Off"
            };
            let mut new_widget = widget_dict.clone();
            new_widget.insert(PdfName::new("AS"), PdfObject::Name(PdfName::new(as_state)));
            self.writer
                .overwrite_object(widget_id, PdfObject::Dict(new_widget));
        }
        Ok(())
    }

    fn collect_button_on_states(&self, field: &FormField) -> Result<Vec<String>> {
        let mut states = Vec::new();
        for &w in &field.widgets {
            let dict = self.writer.resolve_current(w)?.as_dict()?.clone();
            states.extend(on_states_from_widget(&dict, self.writer)?);
        }
        Ok(states
            .into_iter()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect())
    }
}

fn widget_will_have_appearance(field: &FormField, value: &str, rect: Rect) -> bool {
    if !matches!(field.kind, FieldKind::Text | FieldKind::Choice) || field.is_password() {
        return false;
    }
    let display = if field.kind == FieldKind::Choice {
        field
            .options
            .iter()
            .find(|(export, _)| export == value)
            .map(|(_, display)| display.as_str())
            .unwrap_or(value)
    } else {
        value
    };
    let rect = rect.normalize();
    !display.is_empty() && rect.width() > 1.0 && rect.height() > 1.0
}

fn on_states_from_widget(dict: &PdfDict, writer: &IncrementalWriter) -> Result<Vec<String>> {
    let ap = match dict.get("AP") {
        Some(obj) => writer.deref_current(obj),
        None => return Ok(Vec::new()),
    };
    let ap_dict = ap.as_dict()?;
    let n = match ap_dict.get("N") {
        Some(obj) => writer.deref_current(obj),
        None => return Ok(Vec::new()),
    };
    let n_dict = match n.as_dict() {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()), // /N is a stream, not a state dict
    };
    Ok(n_dict
        .0
        .keys()
        .map(|k| k.as_str().to_string())
        .filter(|s| s != "Off")
        .collect())
}

fn rect_from_array(obj: Option<&PdfObject>) -> Result<Rect> {
    let arr = obj
        .ok_or_else(|| invalid_data("/Rect missing"))?
        .as_array()?;
    if arr.len() != 4 {
        return Err(invalid_data("/Rect array must have 4 elements").into());
    }
    let number = |o: &PdfObject| match o {
        PdfObject::Real(f) if f.is_finite() => Ok(*f),
        PdfObject::Integer(i) => Ok(*i as f64),
        PdfObject::Real(_) => Err(invalid_data("/Rect values must be finite").into()),
        _ => Err(zpdf_core::Error::TypeMismatch {
            expected: "number",
            actual: o.type_name(),
        }),
    };
    let nums = [
        number(&arr[0])?,
        number(&arr[1])?,
        number(&arr[2])?,
        number(&arr[3])?,
    ];
    Ok(Rect::new(nums[0], nums[1], nums[2], nums[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf_with_text_field() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R /AcroForm 5 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Annots [4 0 R] >>",
            "<< /Type /Annot /Subtype /Widget /FT /Tx /T (name) /Rect [0 0 100 20] /P 3 0 R /DA (/Helv 12 Tf 0 g) >>",
            "<< /Fields [4 0 R] /DA (/Helv 12 Tf 0 g) >>",
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", index + 1, object).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn form_appearance_exhaustion_does_not_change_field_or_widget() {
        let mut writer = IncrementalWriter::new(pdf_with_text_field()).unwrap();
        writer.next_obj_num = u32::MAX;
        {
            let mut filler = FormFiller::new(&mut writer).unwrap();
            assert!(filler.set("name", "updated").is_err());
        }
        assert!(writer.pending.is_empty());
        assert_eq!(writer.next_obj_num, u32::MAX);
    }
}
