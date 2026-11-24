//! Coarse-grained tagging of an existing untagged PDF (IncrementalWriter).
//!
//! [`IncrementalWriter::tag_pdf`] wraps each page's existing `/Contents` in a
//! single marked-content sequence (`/Part << /MCID 0 >> BDC … EMC`) and emits a
//! matching `/StructTreeRoot` + `/ParentTree` + `/MarkInfo /Marked true`, with
//! one `/Part` structure element per page. The page's `/Alt` carries the
//! best-effort extracted text, giving screen readers *something* to read even
//! for content that was never fine-segmented into paragraphs/figures.
//!
//! # Granularity & limits
//!
//! This is **coarse-grained**: it cannot re-segment existing content streams
//! into semantically meaningful elements (paragraphs, headings, table cells),
//! because that requires understanding the visual layout of already-painted
//! glyphs — out of scope. Each page becomes one `/Part` element wrapping the
//! whole page. This satisfies the *structural* PDF/UA requirement (a valid,
//! non-empty structure tree with marked content) but does not produce
//! heading/paragraph/table semantics. Documents needing fine-grained tags
//! should be authored with [`crate::DocumentBuilder`]'s tagged APIs instead.
//!
//! The method is a no-op (returns `Ok`) when the document is already tagged
//! (`/MarkInfo /Marked true`), so it is safe to call on any PDF.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;

use crate::bounded::BoundedBytes;
use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfString};
use zpdf_document::PdfDocument;

use crate::{invalid_data, IncrementalWriter};

/// A neutral marker role for per-page grouping. `/Part` is the standard
/// block-level grouping structure type (ISO 32000-1 §14.8.4) — a generic
/// container with no finer semantic obligation, the right choice when we
/// cannot infer a page's real structure.
const PAGE_ROLE: &str = "Part";

impl IncrementalWriter {
    /// Add a coarse-grained tag structure to an untagged PDF: wrap each page's
    /// `/Contents` in a `/Part` marked-content sequence and emit a
    /// `/StructTreeRoot` + `/ParentTree` + `/MarkInfo /Marked true`, with one
    /// `/Part` element per page carrying the page's extracted text as `/Alt`.
    ///
    /// No-op when the document is already tagged. The result is a valid Tagged
    /// PDF that passes `zpdf_document::pdfua::validate`'s structural checks,
    /// but the tags are page-level only (no paragraph/heading/table
    /// semantics) — see the module docs for the granularity limit.
    pub fn tag_pdf(&mut self) -> Result<(), zpdf_core::Error> {
        let catalog = self.resolve_current(self.catalog_ref())?;
        if catalog
            .as_dict()?
            .get("MarkInfo")
            .map(|value| self.deref_current(value))
            .and_then(|value| value.as_dict().ok().cloned())
            .is_some_and(|info| matches!(info.get("Marked"), Some(PdfObject::Bool(true))))
        {
            return Ok(());
        }
        let page_count = self.page_count();
        if page_count == 0 {
            return Ok(());
        }

        // Text and resource resolution must see the same revision that will
        // be wrapped below. This private plaintext snapshot is used only for
        // extraction; normal write/sign encryption remains unchanged.
        let snapshot = self.current_tagging_document()?;
        let page_indices = self.page_indices().to_vec();

        // Reserve object numbers up front so every cross-reference (the tree
        // root, per-page elements, the parent-tree array + number-tree) can be
        // fixed before any stream is written. Layout:
        //   per page: 1 element + 1 prefix stream + 1 suffix stream + 1
        //            parent-tree array  = 4 objects
        //   + 1 StructTreeRoot + 1 ParentTree number-tree
        let per_page = 4usize;
        let total = page_count
            .checked_mul(per_page)
            .and_then(|n| n.checked_add(2))
            .ok_or_else(|| invalid_data("too many pages to tag"))?;
        self.ensure_object_capacity(total)?;

        let root = self
            .doc
            .file()
            .trailer
            .get_ref("Root")
            .map_err(|_| invalid_data("trailer missing /Root"))?;

        // --- Phase 1: per-page content wrapping + element/parent-tree nums ---
        let mut page_elem_refs: Vec<ObjectId> = Vec::with_capacity(page_count);
        let mut parent_array_refs: Vec<ObjectId> = Vec::with_capacity(page_count);
        for (idx, page_index) in page_indices.into_iter().enumerate() {
            let page_id = self.page_id(page_index)?;
            let alt = page_text(&snapshot, idx);
            let (prefix_ref, suffix_ref) = self.wrap_page_contents(page_id)?;
            let elem_ref = self.emit_page_element(page_id, alt)?;
            let parent_array_ref = self.emit_parent_array(elem_ref)?;
            page_elem_refs.push(elem_ref);
            parent_array_refs.push(parent_array_ref);
            // Attach /StructParents and the wrapped /Contents to the page dict.
            self.patch_page_for_tagging(page_id, idx, prefix_ref, suffix_ref)?;
        }

        // --- Phase 2: ParentTree number-tree + StructTreeRoot + catalog ---
        let parent_tree_ref = self.emit_parent_tree(&parent_array_refs)?;
        let tree_root_ref = self.emit_struct_tree_root(&page_elem_refs, parent_tree_ref)?;
        self.patch_catalog_for_tagging(root, tree_root_ref)?;

        Ok(())
    }

    /// Serialize the current text-extraction graph to private, bounded memory.
    /// Images, document metadata, attachments and annotations are not needed
    /// for page_text and must not consume the snapshot's byte budget. Keep
    /// page inheritance, Form XObjects and font dependencies unchanged.
    /// resolve_current has already decrypted source objects. A fresh trailer
    /// deliberately omits /Encrypt here, never in the externally saved file.
    fn current_tagging_document(&self) -> Result<PdfDocument, zpdf_core::Error> {
        let limits = self.document().file().limits();
        let mut out = BoundedBytes::new(limits.max_decoded_stream_bytes);
        out.write_all(b"%PDF-1.7\n")?;
        let root = self.catalog_ref();
        let mut queue = vec![root];
        let mut seen = HashSet::from([root]);
        let mut offsets = BTreeMap::new();
        let mut head = 0;
        let mut entries = 0u64;
        while head < queue.len() {
            let id = queue[head];
            head += 1;
            let object = tagging_snapshot_object(self.resolve_current(id)?, id == root);
            let mut direct = vec![&object];
            while let Some(value) = direct.pop() {
                entries += 1;
                if entries > u64::from(limits.max_objects) {
                    return Err(invalid_data("tagging snapshot exceeds object limit").into());
                }
                match value {
                    PdfObject::Ref(id) => {
                        if seen.insert(*id) {
                            queue.push(*id);
                        }
                    }
                    PdfObject::Array(values) => direct.extend(values),
                    PdfObject::Dict(dict) => direct.extend(dict.0.values()),
                    PdfObject::Stream(stream) => direct.extend(stream.dict.0.values()),
                    _ => {}
                }
                if direct.len() as u64 + entries > u64::from(limits.max_objects) {
                    return Err(invalid_data("tagging snapshot exceeds object limit").into());
                }
            }
            offsets.insert(id, out.len());
            match object {
                PdfObject::Stream(stream) => crate::serialize::write_stream(
                    &mut out,
                    id.0,
                    u32::from(id.1),
                    &stream.dict,
                    &stream.data,
                )?,
                object => crate::serialize::write_object(&mut out, id.0, u32::from(id.1), &object)?,
            }
        }
        let xref = out.len();
        out.write_all(b"xref\n0 1\n0000000000 65535 f \n")?;
        for (id, offset) in offsets {
            writeln!(out, "{} 1\n{offset:010} {:05} n ", id.0, id.1)?;
        }
        writeln!(
            out,
            "trailer\n<< /Size {} /Root {} {} R >>\nstartxref\n{xref}\n%%EOF",
            self.next_obj_num, root.0, root.1,
        )?;
        let mut snapshot = PdfDocument::open_with_limits(out.into_inner(), limits.clone())?;
        if let Some(provider) = self.document().font_provider() {
            snapshot.set_font_provider(provider.clone());
        }
        Ok(snapshot)
    }

    /// Wrap a page's `/Contents` in a marked-content sequence. Returns the
    /// (prefix, suffix) stream object ids. The prefix opens
    /// `q /Part <</MCID 0>> BDC`; the suffix closes `EMC Q`. The original
    /// content streams sit between them in the new `/Contents` array.
    fn wrap_page_contents(
        &mut self,
        page_id: ObjectId,
    ) -> Result<(ObjectId, ObjectId), zpdf_core::Error> {
        let page_obj = self.resolve_current(page_id)?;
        let page_dict = page_obj.as_dict()?.clone();
        let prefix = b"q /Part <</MCID 0>> BDC\n";
        let suffix = b"\nEMC Q\n";
        // Rebuild /Contents as [prefix, ...originals, suffix]. Resolve through
        // pending edits as appended pages can refer to a newly copied array.
        let orig = match page_dict.get("Contents") {
            Some(PdfObject::Array(a)) => a.clone(),
            Some(PdfObject::Ref(id)) => match self.resolve_current(*id)? {
                PdfObject::Array(a) => a,
                _ => vec![PdfObject::Ref(*id)],
            },
            _ => Vec::new(),
        };
        let (pnum, _) = self.try_add_stream(&PdfDict::new(), prefix)?;
        let (snum, _) = self.try_add_stream(&PdfDict::new(), suffix)?;
        let mut new_contents = vec![PdfObject::Ref(ObjectId(pnum, 0))];
        new_contents.extend(orig);
        new_contents.push(PdfObject::Ref(ObjectId(snum, 0)));
        let mut new_page = page_dict.clone();
        new_page.insert(PdfName::new("Contents"), PdfObject::Array(new_contents));
        self.overwrite_object(page_id, PdfObject::Dict(new_page));
        Ok((ObjectId(pnum, 0), ObjectId(snum, 0)))
    }

    /// Emit a `/Part` element with Alt extracted from the pending snapshot.
    /// The structure-tree construction supplies its parent reference.
    fn emit_page_element(
        &mut self,
        page_id: ObjectId,
        alt: String,
    ) -> Result<ObjectId, zpdf_core::Error> {
        let (num, _) = self.try_add_object(&PdfObject::Dict(PdfDict::new()))?;
        let elem_id = ObjectId(num, 0);
        // /K and /Pg refer to the current page; /P is attached once the
        // structure-tree root has been allocated.
        let mut elem = PdfDict::new();
        elem.insert(
            PdfName::new("Type"),
            PdfObject::Name(PdfName::new("StructElem")),
        );
        elem.insert(PdfName::new("S"), PdfObject::Name(PdfName::new(PAGE_ROLE)));
        elem.insert(PdfName::new("K"), PdfObject::Integer(0));
        elem.insert(PdfName::new("Pg"), PdfObject::Ref(page_id));
        if !alt.is_empty() {
            elem.insert(
                PdfName::new("Alt"),
                PdfObject::String(PdfString(alt.into_bytes())),
            );
        }
        self.overwrite_object(elem_id, PdfObject::Dict(elem));
        Ok(elem_id)
    }

    /// Emit the per-page parent-tree array object: `[elem_ref]` (one element,
    /// since each page has a single MCID-0 marked-content sequence).
    fn emit_parent_array(&mut self, elem_ref: ObjectId) -> Result<ObjectId, zpdf_core::Error> {
        let arr = PdfObject::Array(vec![PdfObject::Ref(elem_ref)]);
        let (num, _) = self.try_add_object(&arr)?;
        Ok(ObjectId(num, 0))
    }

    /// Patch a page dict with `/StructParents <idx>` (the parent-tree key).
    /// The `/Contents` wrapping was already done in `wrap_page_contents`; here
    /// we only add `/StructParents` to the (already-overwritten) page dict by
    /// re-resolving and re-overwriting.
    fn patch_page_for_tagging(
        &mut self,
        page_id: ObjectId,
        idx: usize,
        _prefix: ObjectId,
        _suffix: ObjectId,
    ) -> Result<(), zpdf_core::Error> {
        let page_obj = self.resolve_current(page_id)?;
        let mut page_dict = page_obj.as_dict()?.clone();
        page_dict.insert(
            PdfName::new("StructParents"),
            PdfObject::Integer(idx as i64),
        );
        self.overwrite_object(page_id, PdfObject::Dict(page_dict));
        Ok(())
    }

    /// Emit the `/ParentTree` number-tree: `<< /Nums [0 arr0 1 arr1 ...] >>`.
    fn emit_parent_tree(&mut self, arrays: &[ObjectId]) -> Result<ObjectId, zpdf_core::Error> {
        let mut nums: Vec<PdfObject> = Vec::with_capacity(arrays.len() * 2);
        for (i, arr) in arrays.iter().enumerate() {
            nums.push(PdfObject::Integer(i as i64));
            nums.push(PdfObject::Ref(*arr));
        }
        let mut dict = PdfDict::new();
        dict.insert(PdfName::new("Nums"), PdfObject::Array(nums));
        let (num, _) = self.try_add_object(&PdfObject::Dict(dict))?;
        Ok(ObjectId(num, 0))
    }

    /// Emit the `/StructTreeRoot` and patch every page element's `/P` to point
    /// at it. Returns the root object id. Capacity was reserved up front, so
    /// the single `try_add_object` cannot fail in practice.
    fn emit_struct_tree_root(
        &mut self,
        page_elems: &[ObjectId],
        parent_tree: ObjectId,
    ) -> Result<ObjectId, zpdf_core::Error> {
        let (num, _) = self.try_add_object(&PdfObject::Dict(PdfDict::new()))?;
        let root_id = ObjectId(num, 0);
        // Patch each element's /P → root.
        for elem in page_elems {
            let obj = self.resolve_current(*elem).unwrap_or(PdfObject::Null);
            if let Ok(d) = obj.as_dict().cloned() {
                let mut d = d;
                d.insert(PdfName::new("P"), PdfObject::Ref(root_id));
                self.overwrite_object(*elem, PdfObject::Dict(d));
            }
        }
        let mut root = PdfDict::new();
        root.insert(
            PdfName::new("Type"),
            PdfObject::Name(PdfName::new("StructTreeRoot")),
        );
        root.insert(
            PdfName::new("K"),
            PdfObject::Array(page_elems.iter().map(|e| PdfObject::Ref(*e)).collect()),
        );
        root.insert(PdfName::new("ParentTree"), PdfObject::Ref(parent_tree));
        root.insert(
            PdfName::new("ParentTreeNextKey"),
            PdfObject::Integer(page_elems.len() as i64),
        );
        self.overwrite_object(root_id, PdfObject::Dict(root));
        Ok(root_id)
    }

    /// Patch the catalog: add `/StructTreeRoot <ref>` and
    /// `/MarkInfo << /Marked true >>`.
    fn patch_catalog_for_tagging(
        &mut self,
        root: ObjectId,
        tree_root: ObjectId,
    ) -> Result<(), zpdf_core::Error> {
        let cat_obj = self.resolve_current(root)?;
        let mut cat = cat_obj.as_dict()?.clone();
        cat.insert(PdfName::new("StructTreeRoot"), PdfObject::Ref(tree_root));
        let mut mark = PdfDict::new();
        mark.insert(PdfName::new("Marked"), PdfObject::Bool(true));
        cat.insert(PdfName::new("MarkInfo"), PdfObject::Dict(mark));
        self.overwrite_object(root, PdfObject::Dict(cat));
        Ok(())
    }
}

/// Project only the private extraction copy; never modify pending objects or
/// the source file. In particular, page_text has no image cache and already
/// skips image drawing, while Form content and font programs can supply text.
fn tagging_snapshot_object(object: PdfObject, is_catalog: bool) -> PdfObject {
    match object {
        PdfObject::Dict(mut dict) if is_catalog => {
            dict.0
                .retain(|key, _| matches!(key.as_str(), "Type" | "Pages"));
            PdfObject::Dict(dict)
        }
        PdfObject::Dict(mut dict) if matches!(dict.get_name("Type"), Ok("Page" | "Pages")) => {
            dict.0.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "Type"
                        | "Parent"
                        | "Kids"
                        | "Count"
                        | "Resources"
                        | "MediaBox"
                        | "CropBox"
                        | "Rotate"
                        | "UserUnit"
                        | "Contents"
                )
            });
            PdfObject::Dict(dict)
        }
        PdfObject::Stream(stream)
            if matches!(stream.dict.get_name("Subtype"), Ok("Image"))
                || matches!(
                    stream.dict.get_name("Type"),
                    Ok("Metadata" | "EmbeddedFile")
                ) =>
        {
            // Do not traverse image masks, ICC profiles or other payload
            // dependencies either. The original objects remain intact on save.
            PdfObject::Null
        }
        other => other,
    }
}

/// Best-effort per-page text extraction for the `/Alt` of each page's `/Part`
/// element. Returns an empty string when extraction is unavailable (no fonts,
/// parse error) so we emit no `/Alt` rather than a misleading one.
fn page_text(doc: &PdfDocument, idx: usize) -> String {
    let Ok(page) = doc.page(idx) else {
        return String::new();
    };
    let mut font_cache = doc.load_page_fonts(&page);
    let Ok(content) = doc.page_content_bytes(&page) else {
        return String::new();
    };
    let mut spans: Vec<zpdf_content::text::TextSpan> = Vec::new();
    {
        let interp = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_fonts(&mut font_cache)
            .with_document(doc.file(), &page.resources)
            .with_shared_fonts(doc)
            .with_text_sink(&mut spans)
            .with_skip_images(true)
            .with_skip_type3_painting(true);
        let _ = interp.interpret(&content);
    }
    let text = zpdf_content::text::spans_to_text(spans, 2.0);
    // Cap the /Alt length so an adversarial or huge page cannot bloat the
    // structure element; 4 KiB is ample for a screen-reader page summary.
    const ALT_CAP: usize = 4096;
    if text.len() <= ALT_CAP {
        text
    } else {
        // Truncate on a char boundary.
        let mut end = ALT_CAP;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use zpdf_core::ParseLimits;

    fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut bytes = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(b"\nendstream");
        bytes
    }

    fn image_heavy_pdf() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let image = vec![0x5a; 32 * 1024];
        let attachment = vec![0x6b; 32 * 1024];
        let objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R /Metadata 11 0 R \
              /Names << /EmbeddedFiles << /Names [(file) 10 0 R] >> >> >>"
                .to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 200] \
              /Resources << /Font << /F1 5 0 R >> \
              /XObject << /Im 6 0 R /Fm 8 0 R >> >> >>"
                .to_vec(),
            b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [9 0 R] \
              /AF [10 0 R] >>"
                .to_vec(),
            stream(
                "",
                b"BT /F1 12 Tf 10 100 Td (Stale page) Tj ET /Im Do /Fm Do",
            ),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            stream(
                "/Type /XObject /Subtype /Image /Width 256 /Height 128 \
                    /ColorSpace /DeviceGray /BitsPerComponent 8",
                &image,
            ),
            stream("/Type /EmbeddedFile", &attachment),
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
                    /Resources << /Font << /F2 5 0 R >> /XObject << /Im 6 0 R >> >>",
                b"BT /F2 12 Tf 10 80 Td (Stale form) Tj ET /Im Do",
            ),
            b"<< /Type /Annot /Subtype /FileAttachment /Rect [0 0 10 10] /FS 10 0 R >>".to_vec(),
            b"<< /Type /Filespec /F (file) /EF << /F 7 0 R >> >>".to_vec(),
            stream("/Type /Metadata /Subtype /XML", &attachment),
        ];
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1,
            )
            .as_bytes(),
        );
        (pdf, image, attachment)
    }

    #[test]
    fn tagging_large_images_keeps_current_page_and_form_text() {
        let (pdf, image, attachment) = image_heavy_pdf();
        let mut writer = IncrementalWriter::new(pdf.clone()).unwrap();
        // A modest budget makes the regression deterministic without a
        // hundreds-of-megabytes fixture. Raw image storage has its own limit.
        let limits = ParseLimits {
            max_decoded_stream_bytes: 8 * 1024,
            ..Default::default()
        };
        writer.doc = PdfDocument::open_with_limits(pdf, limits).unwrap();
        let (content, _) = writer
            .try_add_stream(
                &PdfDict::new(),
                b"BT /F1 12 Tf 10 100 Td (Current page) Tj ET /Im Do /Fm Do",
            )
            .unwrap();
        let mut page = writer
            .resolve_current(ObjectId(3, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        page.insert(
            PdfName::new("Contents"),
            PdfObject::Ref(ObjectId(content, 0)),
        );
        writer.overwrite_object(ObjectId(3, 0), PdfObject::Dict(page));

        let form = writer.resolve_current(ObjectId(8, 0)).unwrap();
        let (form, _) = writer
            .try_add_stream(
                &form.as_stream().unwrap().dict,
                b"BT /F2 12 Tf 10 80 Td (Current form) Tj ET /Im Do",
            )
            .unwrap();
        // Keep resource inheritance while replacing the Form reference in the
        // pending ancestor. Both page content and Form lookup must see edits.
        let mut pages = writer
            .resolve_current(ObjectId(2, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        let mut resources = pages.get("Resources").unwrap().as_dict().unwrap().clone();
        let mut xobjects = resources.get("XObject").unwrap().as_dict().unwrap().clone();
        xobjects.insert(PdfName::new("Fm"), PdfObject::Ref(ObjectId(form, 0)));
        resources.insert(PdfName::new("XObject"), PdfObject::Dict(xobjects));
        pages.insert(PdfName::new("Resources"), PdfObject::Dict(resources));
        writer.overwrite_object(ObjectId(2, 0), PdfObject::Dict(pages));

        writer
            .tag_pdf()
            .expect("large non-text payloads must not exhaust the text snapshot budget");
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();
        let document = PdfDocument::open(output.into_inner()).unwrap();
        let tree = document.struct_tree().unwrap();
        let alt = tree.children[0].alt.as_deref().unwrap();
        assert!(alt.contains("Current page"), "{alt}");
        assert!(alt.contains("Current form"), "{alt}");
        assert!(!alt.contains("Stale"), "{alt}");
        for (id, expected) in [
            (ObjectId(6, 0), &image),
            (ObjectId(7, 0), &attachment),
            (ObjectId(11, 0), &attachment),
        ] {
            let object = document.file().resolve(id).unwrap();
            assert_eq!(
                object.as_stream().unwrap().data.as_ref(),
                expected.as_slice()
            );
        }
        let page = document.page(0).unwrap();
        assert!(page.resources.xobjects.contains_key("Im"));
        assert_eq!(page.annots, [ObjectId(9, 0)]);
    }
}
