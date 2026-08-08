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
        let doc = self.document();
        if is_already_tagged(doc) {
            return Ok(());
        }
        let page_count = doc.page_count();
        if page_count == 0 {
            return Ok(());
        }

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
        for idx in 0..page_count {
            let page_id = self.page_id(idx)?;
            let (prefix_ref, suffix_ref) = self.wrap_page_contents(page_id)?;
            let elem_ref = self.emit_page_element(idx, page_id)?;
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
        let (pnum, _) = self.try_add_stream(&PdfDict::new(), prefix)?;
        let (snum, _) = self.try_add_stream(&PdfDict::new(), suffix)?;
        // Rebuild /Contents as [prefix, ...originals, suffix].
        let orig = match page_dict.get("Contents") {
            Some(PdfObject::Array(a)) => a.clone(),
            Some(r @ PdfObject::Ref(_)) => vec![r.clone()],
            _ => Vec::new(),
        };
        let mut new_contents = vec![PdfObject::Ref(ObjectId(pnum, 0))];
        new_contents.extend(orig);
        new_contents.push(PdfObject::Ref(ObjectId(snum, 0)));
        let mut new_page = page_dict.clone();
        new_page.insert(PdfName::new("Contents"), PdfObject::Array(new_contents));
        self.overwrite_object(page_id, PdfObject::Dict(new_page));
        Ok((ObjectId(pnum, 0), ObjectId(snum, 0)))
    }

    /// Emit a `/Part` `StructElem` for page `idx`. Uses placeholder `/P` and
    /// `/Pg` refs that [`Self::finalize_page_element`] patches once all
    /// numbers are reserved; the `/Alt` carries the page's extracted text.
    fn emit_page_element(
        &mut self,
        idx: usize,
        page_id: ObjectId,
    ) -> Result<ObjectId, zpdf_core::Error> {
        let alt = page_text(self.document(), idx);
        let (num, _) = self.try_add_object(&PdfObject::Dict(PdfDict::new()))?;
        let elem_id = ObjectId(num, 0);
        // Build the element dict with real /K (MCID 0) and /Alt; /P and /Pg are
        // patched in finalize_page_element to avoid ordering coupling.
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

/// True when the document already declares `/MarkInfo /Marked true` — tagging
/// would be redundant (and re-wrapping already-marked content is wrong).
fn is_already_tagged(doc: &PdfDocument) -> bool {
    doc.is_tagged()
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
            .with_text_sink(&mut spans);
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
