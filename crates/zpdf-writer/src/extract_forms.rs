//! Preserve only the field branches owning Widgets on extracted pages.
use crate::{copier, invalid_data, IncrementalWriter, ObjectIdMap};
use std::collections::HashSet;
use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Result};
use zpdf_document::PdfPage;
use zpdf_parser::PdfFile;

fn resolve(file: &PdfFile, obj: &PdfObject) -> Result<PdfObject> {
    match obj {
        PdfObject::Ref(id) => file.resolve(*id),
        obj => Ok(obj.clone()),
    }
}

pub(crate) fn copy_selected_form(
    writer: &mut IncrementalWriter,
    source: &PdfFile,
    pages: &[ObjectId],
    map: &mut ObjectIdMap,
) -> Result<()> {
    let catalog = source.resolve(source.trailer.get_ref("Root")?)?;
    let Some(form) = catalog.as_dict()?.get("AcroForm") else {
        return Ok(());
    };
    let form = resolve(source, form)?;
    let form = form.as_dict()?;
    let Some(fields) = form.get("Fields") else {
        return Ok(());
    };
    let fields = resolve(source, fields)?;
    let mut selected = HashSet::new();
    for &page in pages {
        for annot in PdfPage::from_object(source, page)?.annots {
            if selected.len() >= 65_536 {
                return Err(invalid_data("too many extracted widgets").into());
            }
            if source.resolve(annot)?.as_dict()?.get_name("Subtype").ok() == Some("Widget") {
                selected.insert(annot);
            }
        }
    }
    let mut copier = FieldCopier {
        writer,
        source,
        map,
        selected,
        visiting: HashSet::new(),
        remaining: 65_536,
    };
    let mut roots = Vec::new();
    for field in fields.as_array()? {
        if let PdfObject::Ref(id) = field {
            if let Some(id) = copier.copy(*id, 0)? {
                roots.push(PdfObject::Ref(id));
            }
        }
    }
    if roots.is_empty() {
        return Ok(());
    }
    let mut result = PdfDict::new();
    result.insert(PdfName::new("Fields"), PdfObject::Array(roots));
    // These defaults are inherited by the retained fields. Do not copy /CO or
    // /XFA wholesale: they may reference fields/pages outside the extraction.
    for key in ["DR", "DA", "Q", "NeedAppearances"] {
        if let Some(value) = form.get(key) {
            let value = copier::remap_via_copy(value, source, copier.writer, copier.map, &[])?;
            result.insert(PdfName::new(key), value);
        }
    }
    let catalog_id = copier.writer.catalog_ref();
    let mut catalog = copier
        .writer
        .resolve_current(catalog_id)?
        .as_dict()?
        .clone();
    catalog.insert(PdfName::new("AcroForm"), PdfObject::Dict(result));
    copier
        .writer
        .overwrite_object(catalog_id, PdfObject::Dict(catalog));
    Ok(())
}

struct FieldCopier<'a> {
    writer: &'a mut IncrementalWriter,
    source: &'a PdfFile,
    map: &'a mut ObjectIdMap,
    selected: HashSet<ObjectId>,
    visiting: HashSet<ObjectId>,
    remaining: usize,
}
impl FieldCopier<'_> {
    fn copy(&mut self, id: ObjectId, depth: usize) -> Result<Option<ObjectId>> {
        if depth >= 50 || self.remaining == 0 || !self.visiting.insert(id) {
            return Err(invalid_data("cyclic or excessive extracted field tree").into());
        }
        self.remaining -= 1;
        let obj = self.source.resolve(id)?;
        let mut dict = obj.as_dict()?.clone();
        let mut kids = Vec::new();
        if let Some(raw) = dict.get("Kids") {
            let raw = resolve(self.source, raw)?;
            for kid in raw.as_array()? {
                if let PdfObject::Ref(child) = kid {
                    if let Some(child) = self.copy(*child, depth + 1)? {
                        kids.push(child);
                    }
                }
            }
        }
        self.visiting.remove(&id);
        if !self.selected.contains(&id) && kids.is_empty() {
            return Ok(None);
        }
        dict.0.remove(&PdfName::new("Parent"));
        dict.0.remove(&PdfName::new("Kids"));
        let dest = match self.map.get(id) {
            Some(dest) => dest,
            None => copier::reserve(self.writer, id, self.map)?,
        };
        let obj = copier::remap_via_copy(
            &PdfObject::Dict(dict),
            self.source,
            self.writer,
            self.map,
            &["Parent"],
        )?;
        let mut dict = obj.as_dict()?.clone();
        if !kids.is_empty() {
            dict.insert(
                PdfName::new("Kids"),
                PdfObject::Array(kids.iter().copied().map(PdfObject::Ref).collect()),
            );
        }
        self.writer.overwrite_object(dest, PdfObject::Dict(dict));
        for kid in kids {
            let mut dict = self.writer.resolve_current(kid)?.as_dict()?.clone();
            dict.insert(PdfName::new("Parent"), PdfObject::Ref(dest));
            self.writer.overwrite_object(kid, PdfObject::Dict(dict));
        }
        Ok(Some(dest))
    }
}
