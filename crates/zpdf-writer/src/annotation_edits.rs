//! Regenerate the normal appearance from the current annotation properties.
use crate::{invalid_data, unsupported, IncrementalWriter};
use zpdf_core::{Matrix, ObjectId, PdfDict, PdfName, PdfObject, Rect, Result};
use zpdf_document::{forms::GeneratedAppearance, FieldKind, FieldValue, FormField};

impl IncrementalWriter {
    pub(crate) fn regenerate_annotation_appearance(
        &mut self,
        dict: &mut PdfDict,
        extra_objects: usize,
    ) -> Result<()> {
        let subtype = dict.get_name("Subtype")?.to_string();
        if !matches!(
            subtype.as_str(),
            "Highlight"
                | "Underline"
                | "StrikeOut"
                | "Squiggly"
                | "Square"
                | "Circle"
                | "Line"
                | "Polygon"
                | "PolyLine"
                | "Ink"
                | "FreeText"
                | "Link"
                | "Text"
                | "Caret"
                | "Redact"
        ) {
            return Err(
                unsupported("cannot regenerate this annotation's custom appearance").into(),
            );
        }
        let mut inputs = dict.clone();
        let mut budget = 65_536;
        for key in [
            "Rect",
            "C",
            "IC",
            "BS",
            "Border",
            "QuadPoints",
            "Vertices",
            "L",
            "InkList",
            "DA",
            "Contents",
            "Q",
            "CA",
            "RD",
            "CL",
            "LE",
            "Name",
        ] {
            if let Some(value) = inputs.get(key) {
                let value = self.appearance_input(value, 0, &mut budget)?;
                inputs.insert(PdfName::new(key), value);
            }
        }
        let rect = crate::annotate::rect_from_dict(&inputs)
            .ok_or_else(|| invalid_data("annotation has no usable Rect"))?
            .normalize();
        if ![rect.x0, rect.y0, rect.x1, rect.y1]
            .iter()
            .all(|n| n.is_finite())
            || rect.width() <= 0.0
            || rect.height() <= 0.0
            || !rect.width().is_finite()
            || !rect.height().is_finite()
        {
            return Err(invalid_data("annotation Rect must be finite and non-empty").into());
        }
        let text = match inputs.get("Contents") {
            Some(PdfObject::String(s)) => zpdf_core::decode_text_string(s.as_bytes()),
            _ => String::new(),
        };
        let mut prepared = None;
        let mut origin = (rect.x0, rect.y0);
        if subtype == "FreeText" && !text.is_empty() {
            if text.chars().count() > 16_384 {
                return Err(invalid_data("FreeText appearance exceeds 16384 characters").into());
            }
            if crate::unicode_text::requires_unicode(&text) || self.doc.font_provider().is_some() {
                let mut insets = [0.0; 4];
                if let Ok(rd) = inputs.get_array("RD") {
                    if rd.len() == 4 {
                        for (out, obj) in insets.iter_mut().zip(rd) {
                            *out = obj
                                .as_f64()
                                .ok()
                                .filter(|v| v.is_finite() && *v >= 0.0)
                                .unwrap_or(0.0);
                        }
                    }
                }
                let inner = Rect::new(
                    0.0,
                    0.0,
                    (rect.width() - insets[0] - insets[2]).max(1.0),
                    (rect.height() - insets[1] - insets[3]).max(1.0),
                );
                origin = (rect.x0 + insets[0], rect.y0 + insets[3]);
                let field = FormField {
                    field_id: ObjectId(0, 0),
                    name: String::new(),
                    kind: FieldKind::Text,
                    flags: zpdf_document::forms::FF_MULTILINE,
                    value: Some(FieldValue::Text(text)),
                    default_appearance: inputs.get("DA").and_then(|v| match v {
                        PdfObject::String(s) => Some(zpdf_core::decode_text_string(s.as_bytes())),
                        _ => None,
                    }),
                    quadding: inputs.get_i64("Q").unwrap_or(0),
                    max_len: None,
                    options: vec![],
                    widgets: vec![],
                };
                prepared = self.prepare_unicode_field(&field, inner, None)?;
                // Keep callouts/background/borders; the embedded child Form
                // supplies the text with its own font resources and clipping.
                inputs.insert(
                    PdfName::new("Contents"),
                    PdfObject::String(zpdf_core::encode_text_string("")),
                );
            }
        }
        let mut ap = zpdf_document::annot_appearance::generate_annotation_appearance(
            self.doc.file(),
            &inputs,
            &subtype,
            rect,
        )
        .unwrap_or(GeneratedAppearance {
            bbox: rect,
            matrix: Matrix::identity(),
            resources: PdfDict::new(),
            content: b"q Q\n".to_vec(),
        });
        self.ensure_object_capacity(
            prepared.as_ref().map_or(1, |p| p.object_budget() + 1) + extra_objects,
        )?;
        if let Some(prepared) = prepared {
            let text_id = self.import_text_form(&prepared)?;
            let mut xobjects = PdfDict::new();
            xobjects.insert(PdfName::new("ZUnicode"), PdfObject::Ref(text_id));
            ap.resources
                .insert(PdfName::new("XObject"), PdfObject::Dict(xobjects));
            let mut prefix = String::from("q\n");
            if let Ok(ca) = inputs.get_f64("CA") {
                let mut gs = PdfDict::new();
                for key in ["ca", "CA"] {
                    gs.insert(PdfName::new(key), PdfObject::Real(ca.clamp(0.0, 1.0)));
                }
                let mut states = ap
                    .resources
                    .get("ExtGState")
                    .and_then(|o| o.as_dict().ok())
                    .cloned()
                    .unwrap_or_default();
                states.insert(PdfName::new("ZTextAlpha"), PdfObject::Dict(gs));
                ap.resources
                    .insert(PdfName::new("ExtGState"), PdfObject::Dict(states));
                prefix.push_str("/ZTextAlpha gs\n");
            }
            ap.content.extend_from_slice(
                format!(
                    "{prefix}1 0 0 1 {} {} cm /ZUnicode Do Q\n",
                    origin.0, origin.1
                )
                .as_bytes(),
            );
        }
        let mut form = PdfDict::new();
        form.insert(
            PdfName::new("Type"),
            PdfObject::Name(PdfName::new("XObject")),
        );
        form.insert(
            PdfName::new("Subtype"),
            PdfObject::Name(PdfName::new("Form")),
        );
        form.insert(
            PdfName::new("BBox"),
            numbers(&[ap.bbox.x0, ap.bbox.y0, ap.bbox.x1, ap.bbox.y1]),
        );
        let m = ap.matrix;
        form.insert(
            PdfName::new("Matrix"),
            numbers(&[m.a, m.b, m.c, m.d, m.e, m.f]),
        );
        form.insert(PdfName::new("Resources"), PdfObject::Dict(ap.resources));
        let (num, gen) = self.try_add_stream(&form, &ap.content)?;
        let mut appearance = PdfDict::new();
        appearance.insert(PdfName::new("N"), PdfObject::Ref(ObjectId(num, gen as u16)));
        dict.insert(PdfName::new("AP"), PdfObject::Dict(appearance));
        Ok(())
    }

    pub(crate) fn move_annotation_geometry(&self, dict: &mut PdfDict, rect: Rect) -> Result<()> {
        if ![
            rect.x0,
            rect.y0,
            rect.x1,
            rect.y1,
            rect.width(),
            rect.height(),
        ]
        .iter()
        .all(|v| v.is_finite())
            || rect.width() <= 0.0
            || rect.height() <= 0.0
        {
            return Err(invalid_data("annotation Rect must be finite and non-empty").into());
        }
        let mut budget = 65_536;
        let old_value = dict
            .get("Rect")
            .ok_or_else(|| invalid_data("annotation has no Rect"))?;
        let old_value = self.appearance_input(old_value, 0, &mut budget)?;
        let mut old_dict = PdfDict::new();
        old_dict.insert(PdfName::new("Rect"), old_value);
        let old = crate::annotate::rect_from_dict(&old_dict)
            .ok_or_else(|| invalid_data("invalid annotation Rect"))?
            .normalize();
        if old.width() <= 0.0 || old.height() <= 0.0 {
            return Err(invalid_data("empty annotation Rect").into());
        }
        let sx = rect.width() / old.width();
        let sy = rect.height() / old.height();
        if ![old.width(), old.height(), sx, sy]
            .iter()
            .all(|v| v.is_finite())
        {
            return Err(
                invalid_data("annotation geometry scale exceeds finite coordinates").into(),
            );
        }
        fn remap(value: &mut PdfObject, old: Rect, new: Rect, sx: f64, sy: f64) {
            if let PdfObject::Array(values) = value {
                if values.iter().all(|v| v.as_f64().is_ok()) {
                    for (i, obj) in values.iter_mut().enumerate() {
                        if let Ok(v) = obj.as_f64() {
                            *obj = PdfObject::Real(if i % 2 == 0 {
                                new.x0 + (v - old.x0) * sx
                            } else {
                                new.y0 + (v - old.y0) * sy
                            });
                        }
                    }
                } else {
                    for obj in values {
                        remap(obj, old, new, sx, sy);
                    }
                }
            }
        }
        for key in ["QuadPoints", "Vertices", "L", "InkList", "CL"] {
            if let Some(value) = dict.get(key) {
                let mut value = self.appearance_input(value, 0, &mut budget)?;
                remap(&mut value, old, rect, sx, sy);
                // Detect overflow before any annotation dictionary is stored.
                value = self.appearance_input(&value, 0, &mut budget)?;
                dict.insert(PdfName::new(key), value);
            }
        }
        if let Some(rd) = dict.get("RD") {
            let mut rd = self.appearance_input(rd, 0, &mut budget)?;
            if let PdfObject::Array(values) = &mut rd {
                for (i, obj) in values.iter_mut().enumerate() {
                    if let Ok(v) = obj.as_f64() {
                        *obj = PdfObject::Real(v * if i % 2 == 0 { sx } else { sy });
                    }
                }
            }
            rd = self.appearance_input(&rd, 0, &mut budget)?;
            dict.insert(PdfName::new("RD"), rd);
        }
        Ok(())
    }

    fn appearance_input(
        &self,
        obj: &PdfObject,
        depth: usize,
        budget: &mut usize,
    ) -> Result<PdfObject> {
        if depth > 16 || *budget == 0 {
            return Err(invalid_data("annotation appearance inputs exceed limits").into());
        }
        *budget -= 1;
        Ok(match obj {
            PdfObject::Real(v) if !v.is_finite() => {
                return Err(invalid_data("non-finite annotation property").into())
            }
            PdfObject::Ref(id) => {
                self.appearance_input(&self.resolve_current(*id)?, depth + 1, budget)?
            }
            PdfObject::Array(a) => PdfObject::Array(
                a.iter()
                    .map(|o| self.appearance_input(o, depth + 1, budget))
                    .collect::<Result<_>>()?,
            ),
            PdfObject::Dict(d) => {
                let mut result = PdfDict::new();
                for (key, value) in &d.0 {
                    result.insert(
                        key.clone(),
                        self.appearance_input(value, depth + 1, budget)?,
                    );
                }
                PdfObject::Dict(result)
            }
            other => other.clone(),
        })
    }
}

fn numbers(values: &[f64]) -> PdfObject {
    PdfObject::Array(values.iter().copied().map(PdfObject::Real).collect())
}
