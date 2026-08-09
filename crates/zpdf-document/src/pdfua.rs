//! PDF/UA conformance validation (profiles UA-1 and UA-2).
//!
//! A rule engine over the parsed document, mirroring [`crate::pdfa`]: each
//! check inspects one aspect and yields zero or more [`Violation`]s. This
//! covers the high-signal, machine-checkable clauses of PDF/UA-1 (ISO 14289-1,
//! the "Universal Accessibility" conformance) and PDF/UA-2 (ISO 14289-2, the
//! PDF 2.0 accessibility standard):
//!
//! - the document is tagged (`/MarkInfo /Marked true`)
//! - a `/StructTreeRoot` is present and non-empty
//! - the catalog declares a `/Lang` (BCP 47)
//! - every `/Figure` structure element carries `/Alt` or `/ActualText`
//! - the structure tree contains at least one heading (`H` / `H1`…`H6`)
//! - every structure role is a standard type or mapped onto one via `/RoleMap`
//!   (no unresolved `Other` roles)
//! - every page carries `/StructParents`
//!
//! Everything is best-effort and read-only over `ParseLimits`-bounded APIs.
//! Annotation `/OBJR` coverage and table-structure consistency are out of
//! scope (first version).

use std::collections::HashSet;

use zpdf_core::{ObjectId, PdfObject};
use zpdf_parser::PdfFile;

use crate::catalog::Catalog;
use crate::structure::{
    is_tagged, parse_struct_tree, StructElem, StructKid, StructRole, StructTree,
};

/// The validation profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// ISO 14289-1 (PDF/UA-1).
    Ua1,
    /// ISO 14289-2 (PDF/UA-2): PDF/UA-1's structural rules, based on PDF 2.0.
    /// The only UA-2-specific check is a PDF 2.0 header; every UA-1 structural
    /// check applies unchanged.
    Ua2,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Ua1 => "PDF/UA-1",
            Profile::Ua2 => "PDF/UA-2",
        }
    }
}

/// One conformance violation.
#[derive(Debug, Clone)]
pub struct Violation {
    /// Short rule identifier, e.g. `"tagged"`, `"figure-alt"`.
    pub rule: &'static str,
    /// Human-readable explanation.
    pub message: String,
}

/// The outcome of a validation run.
#[derive(Debug)]
pub struct ValidationReport {
    pub profile: Profile,
    pub violations: Vec<Violation>,
}

impl ValidationReport {
    pub fn conforms(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Validate `file` against `profile` (PDF/UA-1 or PDF/UA-2).
pub fn validate(file: &PdfFile, profile: Profile) -> ValidationReport {
    let mut v: Vec<Violation> = Vec::new();
    check_tagged(file, &mut v);
    let tree = check_struct_tree(file, &mut v);
    check_lang(file, &mut v);
    if let Some(tree) = &tree {
        check_figures_have_alt(tree, &mut v);
        check_has_heading(tree, &mut v);
        check_roles_standard(tree, &mut v);
        check_table_structure(tree, &mut v);
    }
    check_page_struct_parents(file, &mut v);
    check_annotation_objr(file, tree.as_ref(), &mut v);
    if profile == Profile::Ua2 {
        check_pdf2_header(file, &mut v);
    }
    ValidationReport {
        profile,
        violations: v,
    }
}

/// PDF/UA-2 is based on PDF 2.0 (ISO 32000-2). A header that is not PDF 2.0 is
/// non-conformant. (PDF/UA-1 has no header constraint.)
fn check_pdf2_header(file: &PdfFile, out: &mut Vec<Violation>) {
    let h = file.header;
    if h.major != 2 {
        out.push(Violation {
            rule: "header-version",
            message: format!(
                "header declares PDF {}.{}; PDF/UA-2 is based on PDF 2.0",
                h.major, h.minor
            ),
        });
    }
}

fn check_tagged(file: &PdfFile, out: &mut Vec<Violation>) {
    if !is_tagged(file) {
        out.push(Violation {
            rule: "tagged",
            message: "document is not tagged (/MarkInfo /Marked true absent); PDF/UA requires a tagged PDF".into(),
        });
    }
}

fn check_struct_tree(file: &PdfFile, out: &mut Vec<Violation>) -> Option<StructTree> {
    let Ok(catalog) = Catalog::from_trailer(file) else {
        out.push(Violation {
            rule: "struct-tree",
            message: "catalog cannot be built; no structure tree".into(),
        });
        return None;
    };
    match parse_struct_tree(file, &catalog) {
        Some(tree) => {
            if tree.element_count() == 0 {
                out.push(Violation {
                    rule: "struct-tree",
                    message: "/StructTreeRoot is present but has no structure elements".into(),
                });
                return None;
            }
            Some(tree)
        }
        None => {
            out.push(Violation {
                rule: "struct-tree",
                message: "no /StructTreeRoot; PDF/UA requires a structure tree".into(),
            });
            None
        }
    }
}

fn check_lang(file: &PdfFile, out: &mut Vec<Violation>) {
    let Some(root) = crate::obj_util::catalog_dict(file) else {
        out.push(Violation {
            rule: "lang",
            message: "catalog cannot be read; /Lang cannot be verified".into(),
        });
        return;
    };
    if root.get("Lang").is_none() {
        out.push(Violation {
            rule: "lang",
            message: "catalog has no /Lang; PDF/UA requires a natural-language declaration".into(),
        });
    }
}

/// Walk the structure tree; every `Figure` must carry `/Alt` or `/ActualText`.
fn check_figures_have_alt(tree: &StructTree, out: &mut Vec<Violation>) {
    let mut missing = 0usize;
    visit(tree.children.iter(), &mut |elem| {
        if elem.role == StructRole::Figure && elem.accessible_text().is_none() {
            missing += 1;
        }
    });
    if missing > 0 {
        out.push(Violation {
            rule: "figure-alt",
            message: format!("{missing} Figure element(s) lack /Alt and /ActualText; PDF/UA requires alternative text for figures"),
        });
    }
}

/// The tree must contain at least one heading.
fn check_has_heading(tree: &StructTree, out: &mut Vec<Violation>) {
    let mut has_heading = false;
    visit(tree.children.iter(), &mut |elem| {
        if elem.role.is_heading() {
            has_heading = true;
        }
    });
    if !has_heading {
        out.push(Violation {
            rule: "headings",
            message: "structure tree has no heading (H / H1–H6); PDF/UA requires heading-based document structure".into(),
        });
    }
}

/// No element may carry an unresolved (non-standard, unmapped) role.
fn check_roles_standard(tree: &StructTree, out: &mut Vec<Violation>) {
    let mut bad: Vec<String> = Vec::new();
    visit(tree.children.iter(), &mut |elem| {
        if let StructRole::Other(name) = &elem.role {
            bad.push(name.clone());
        }
    });
    if !bad.is_empty() {
        out.push(Violation {
            rule: "role-unmapped",
            message: format!(
                "structure role(s) not mapped to a standard type via /RoleMap: {}",
                bad.join(", ")
            ),
        });
    }
}

/// Table-structure consistency (ISO 14289-1 §7.6): a `Table` element's
/// element children should be `TR` rows, and a `TR`'s element children should
/// be `TH`/`TD` cells. A `Table` with non-row element children, or a `TR` with
/// non-cell element children, is flagged. Best-effort: only checks element
/// kids (marked-content/OBJR kids inside a table are non-conformant but rare;
/// nesting depth is bounded by the tree's parse-time guards).
fn check_table_structure(tree: &StructTree, out: &mut Vec<Violation>) {
    let mut table_bad = 0usize;
    let mut row_bad = 0usize;
    visit(tree.children.iter(), &mut |elem| {
        if elem.role == StructRole::Table {
            for kid in elem.child_elements() {
                if kid.role != StructRole::Tr {
                    table_bad += 1;
                    break;
                }
            }
        }
        if elem.role == StructRole::Tr {
            for kid in elem.child_elements() {
                if !matches!(kid.role, StructRole::Th | StructRole::Td) {
                    row_bad += 1;
                    break;
                }
            }
        }
    });
    if table_bad > 0 {
        out.push(Violation {
            rule: "table-structure",
            message: format!(
                "{table_bad} Table element(s) have non-TR element children; PDF/UA requires table rows"
            ),
        });
    }
    if row_bad > 0 {
        out.push(Violation {
            rule: "table-structure",
            message: format!(
                "{row_bad} TR element(s) have non-TH/TD element children; PDF/UA requires table cells"
            ),
        });
    }
}

/// Annotation structure coverage (ISO 14289-1 §7.18): annotations that convey
/// content (Widget form fields, Link, and content-bearing markup annotations)
/// should participate in the structure tree via `/OBJR` references. This is a
/// best-effort check: if the document has any such annotations but the
/// structure tree contains no `OBJR` kids at all, flag it. A full per-annotation
/// audit is out of scope (it needs annotation↔page↔OBJR cross-referencing).
fn check_annotation_objr(file: &PdfFile, tree: Option<&StructTree>, out: &mut Vec<Violation>) {
    let Some(tree) = tree else {
        return;
    };
    let Ok(root) = file.trailer.get_ref("Root") else {
        return;
    };
    let Ok(catalog) = file.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return;
    };
    let Ok(pages_root) = catalog.get_ref("Pages") else {
        return;
    };
    // Count content-bearing annotations across all pages.
    let mut content_annots = 0usize;
    let mut stack = vec![(pages_root, 0usize)];
    let mut visited: HashSet<ObjectId> = HashSet::new();
    while let Some((node, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(node) {
            continue;
        }
        let Ok(dict) = file.resolve(node).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(file, o)).as_ref() {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push((*r, depth + 1));
                }
            }
        }
        let annots_obj = dict.get("Annots").map(|o| deref(file, o));
        let Some(PdfObject::Array(annots)) = annots_obj.as_ref() else {
            continue;
        };
        for a in annots {
            let Ok(ad) = deref(file, a).as_dict().cloned() else {
                continue;
            };
            let subtype = ad.get_name("Subtype").unwrap_or("");
            // Widget (form fields), Link, and the markup/content annotations
            // are the ones PDF/UA requires to be structure-reachable.
            if matches!(
                subtype,
                "Widget"
                    | "Link"
                    | "FreeText"
                    | "Text"
                    | "Highlight"
                    | "Underline"
                    | "StrikeOut"
                    | "Squiggly"
            ) {
                content_annots += 1;
            }
        }
    }
    if content_annots == 0 {
        return;
    }
    // Count OBJR kids across the whole structure tree.
    let mut objr_count = 0usize;
    visit_kids(tree.children.iter(), &mut |elem| {
        for kid in &elem.kids {
            if matches!(kid, StructKid::Object { .. }) {
                objr_count += 1;
            }
        }
    });
    if objr_count == 0 {
        out.push(Violation {
            rule: "annotation-objr",
            message: format!(
                "document has {content_annots} content-bearing annotation(s) but the structure tree has no /OBJR references; PDF/UA requires annotations to be structure-reachable"
            ),
        });
    }
}

/// Every page dict must carry `/StructParents`.
fn check_page_struct_parents(file: &PdfFile, out: &mut Vec<Violation>) {
    let Ok(root) = file.trailer.get_ref("Root") else {
        return;
    };
    let Ok(catalog) = file.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return;
    };
    let Ok(pages_root) = catalog.get_ref("Pages") else {
        return;
    };
    let mut stack = vec![(pages_root, 0usize)];
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut missing = 0usize;
    while let Some((node, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(node) {
            continue;
        }
        let Ok(dict) = file.resolve(node).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        // A leaf page (has /MediaBox or no /Kids) must carry /StructParents.
        let is_leaf = dict.get("Kids").is_none();
        if is_leaf && dict.get("StructParents").is_none() {
            missing += 1;
        }
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(file, o)).as_ref() {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push((*r, depth + 1));
                }
            }
        }
    }
    if missing > 0 {
        out.push(Violation {
            rule: "page-struct-parents",
            message: format!("{missing} page(s) lack /StructParents; PDF/UA requires every page to participate in the structure tree"),
        });
    }
}

/// Depth-first visit over a structure-tree forest, calling `f` on every
/// element (including nested ones). Bounded by the tree's own guards at parse
/// time, so this walk cannot recurse without bound.
fn visit<'a>(elems: impl Iterator<Item = &'a StructElem>, f: &mut dyn FnMut(&StructElem)) {
    fn walk<'a>(elem: &'a StructElem, f: &mut dyn FnMut(&StructElem)) {
        f(elem);
        for child in elem.child_elements() {
            walk(child, f);
        }
    }
    for elem in elems {
        walk(elem, f);
    }
}

/// Like [`visit`], but the callback can inspect each element's `kids` (the
/// `/K` entries, including non-element kids such as `OBJR` references).
fn visit_kids<'a>(elems: impl Iterator<Item = &'a StructElem>, f: &mut dyn FnMut(&StructElem)) {
    fn walk<'a>(elem: &'a StructElem, f: &mut dyn FnMut(&StructElem)) {
        f(elem);
        for child in elem.child_elements() {
            walk(child, f);
        }
    }
    for elem in elems {
        walk(elem, f);
    }
}

fn deref(file: &PdfFile, obj: &PdfObject) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => file.resolve(*r).unwrap_or(PdfObject::Null),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;

    const PAGES: &str = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    const PAGE: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /StructParents 0 >>";

    fn open(objects: &[&str]) -> PdfFile {
        PdfFile::parse(build_pdf(objects)).expect("parse pdf")
    }

    /// Like [`open`] but with a PDF 2.0 header (for PDF/UA-2 tests).
    fn open_v2(objects: &[&str]) -> PdfFile {
        PdfFile::parse(crate::test_util::build_pdf_with_version(2, 0, objects)).expect("parse pdf")
    }

    fn ua_pdf(catalog: &str, extra: &[&str]) -> PdfFile {
        let mut objs = vec![catalog, PAGES, PAGE];
        objs.extend_from_slice(extra);
        open(&objs)
    }

    /// Like [`ua_pdf`] but with a PDF 2.0 header (for PDF/UA-2 tests).
    fn ua_pdf_v2(catalog: &str, extra: &[&str]) -> PdfFile {
        let mut objs = vec![catalog, PAGES, PAGE];
        objs.extend_from_slice(extra);
        open_v2(&objs)
    }

    #[test]
    fn untagged_pdf_fails() {
        let file = ua_pdf("<< /Type /Catalog /Pages 2 0 R >>", &[]);
        let r = validate(&file, Profile::Ua1);
        let rules: Vec<&str> = r.violations.iter().map(|v| v.rule).collect();
        assert!(rules.contains(&"tagged"));
        assert!(rules.contains(&"struct-tree"));
        assert!(rules.contains(&"lang"));
        assert!(!r.conforms());
    }

    #[test]
    fn compliant_tagged_pdf_passes() {
        // StructTreeRoot → Document → [H1 (mcid 0), P (mcid 1)], /Lang set,
        // /MarkInfo marked, page carries /StructParents.
        let file = ua_pdf(
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> \
             /StructTreeRoot 4 0 R >>",
            &[
                // 4: StructTreeRoot
                "<< /Type /StructTreeRoot /K 5 0 R /ParentTree 9 0 R /ParentTreeNextKey 1 >>",
                // 5: Document
                "<< /Type /StructElem /S /Document /P 4 0 R /K [6 0 R 7 0 R] >>",
                // 6: H1
                "<< /Type /StructElem /S /H1 /P 5 0 R /Pg 3 0 R /K 0 >>",
                // 7: P
                "<< /Type /StructElem /S /P /P 5 0 R /Pg 3 0 R /K 1 >>",
                // 8: parent-tree array (H1, P in MCID order)
                "<< 6 0 R 7 0 R >>",
                // 9: ParentTree number tree
                "<< /Nums [0 8 0 R] >>",
            ],
        );
        let r = validate(&file, Profile::Ua1);
        assert!(
            r.conforms(),
            "expected conformance, got: {:?}",
            r.violations
        );
    }

    #[test]
    fn figure_without_alt_is_flagged() {
        let file = ua_pdf(
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> \
             /StructTreeRoot 4 0 R >>",
            &[
                // 4: StructTreeRoot with two top-level elements (H1, Figure).
                "<< /Type /StructTreeRoot /K [5 0 R 6 0 R] /ParentTree 7 0 R /ParentTreeNextKey 1 >>",
                // 5: H1
                "<< /Type /StructElem /S /H1 /P 4 0 R /Pg 3 0 R /K 0 >>",
                // 6: Figure without /Alt or /ActualText
                "<< /Type /StructElem /S /Figure /P 4 0 R /Pg 3 0 R /K 1 >>",
                // 7: ParentTree
                "<< /Nums [0 8 0 R] >>",
                // 8: array
                "<< 5 0 R 6 0 R >>",
            ],
        );
        let r = validate(&file, Profile::Ua1);
        let rules: Vec<&str> = r.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"figure-alt"),
            "figure-alt should fire: {rules:?}"
        );
    }

    #[test]
    fn no_heading_is_flagged() {
        let file = ua_pdf(
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> \
             /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R /ParentTree 6 0 R /ParentTreeNextKey 1 >>",
                "<< /Type /StructElem /S /P /P 4 0 R /Pg 3 0 R /K 0 >>",
                "<< /Nums [0 7 0 R] >>",
                "<< 5 0 R >>",
            ],
        );
        let r = validate(&file, Profile::Ua1);
        let rules: Vec<&str> = r.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"headings"),
            "headings should fire: {rules:?}"
        );
    }

    #[test]
    fn table_with_non_tr_children_is_flagged() {
        // Table whose child is a P (not a TR) — must flag table-structure.
        let file = ua_pdf(
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> \
             /StructTreeRoot 4 0 R >>",
            &[
                // 4: StructTreeRoot → [H1, Table]
                "<< /Type /StructTreeRoot /K [5 0 R 6 0 R] /ParentTree 7 0 R /ParentTreeNextKey 2 >>",
                // 5: H1
                "<< /Type /StructElem /S /H1 /P 4 0 R /Pg 3 0 R /K 0 >>",
                // 6: Table with a P child (non-TR) — non-conformant
                "<< /Type /StructElem /S /Table /P 4 0 R /K [8 0 R] >>",
                // 7: ParentTree
                "<< /Nums [0 9 0 R] >>",
                // 8: P inside the table
                "<< /Type /StructElem /S /P /P 6 0 R /Pg 3 0 R /K 1 >>",
                // 9: array
                "<< 5 0 R 8 0 R >>",
            ],
        );
        let r = validate(&file, Profile::Ua1);
        let rules: Vec<&str> = r.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"table-structure"),
            "table-structure should fire for non-TR table child: {rules:?}"
        );
    }

    #[test]
    fn tr_with_non_cell_children_is_flagged() {
        // Table → TR → P (non-cell) — flags the TR row check.
        let file = ua_pdf(
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> \
             /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K [5 0 R 6 0 R] /ParentTree 7 0 R /ParentTreeNextKey 2 >>",
                // 5: H1
                "<< /Type /StructElem /S /H1 /P 4 0 R /Pg 3 0 R /K 0 >>",
                // 6: Table → TR → P
                "<< /Type /StructElem /S /Table /P 4 0 R /K [8 0 R] >>",
                // 7: ParentTree
                "<< /Nums [0 9 0 R] >>",
                // 8: TR with a P child (non-cell)
                "<< /Type /StructElem /S /TR /P 6 0 R /K [10 0 R] >>",
                // 9: array
                "<< 5 0 R >>",
                // 10: P inside the TR
                "<< /Type /StructElem /S /P /P 8 0 R /Pg 3 0 R /K 1 >>",
            ],
        );
        let r = validate(&file, Profile::Ua1);
        let rules: Vec<&str> = r.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"table-structure"),
            "table-structure should fire for non-cell TR child: {rules:?}"
        );
    }

    #[test]
    fn well_formed_table_passes() {
        // Table → TR → [TH, TD] — conformant table structure.
        let file = ua_pdf(
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> \
             /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K [5 0 R 6 0 R] /ParentTree 7 0 R /ParentTreeNextKey 3 >>",
                // 5: H1
                "<< /Type /StructElem /S /H1 /P 4 0 R /Pg 3 0 R /K 0 >>",
                // 6: Table → TR → [TH, TD]
                "<< /Type /StructElem /S /Table /P 4 0 R /K [8 0 R] >>",
                // 7: ParentTree
                "<< /Nums [0 9 0 R] >>",
                // 8: TR → [TH, TD]
                "<< /Type /StructElem /S /TR /P 6 0 R /K [10 0 R 11 0 R] >>",
                // 9: array
                "<< 5 0 R >>",
                // 10: TH
                "<< /Type /StructElem /S /TH /P 8 0 R /Pg 3 0 R /K 1 >>",
                // 11: TD
                "<< /Type /StructElem /S /TD /P 8 0 R /Pg 3 0 R /K 2 >>",
            ],
        );
        let r = validate(&file, Profile::Ua1);
        let rules: Vec<&str> = r.violations.iter().map(|v| v.rule).collect();
        assert!(
            !rules.contains(&"table-structure"),
            "well-formed table should not flag: {rules:?}"
        );
    }

    #[test]
    fn content_annotation_without_objr_is_flagged() {
        // A page with a Link annotation but no /OBJR in the structure tree.
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /StructParents 0 \
                    /Annots [10 0 R] >>";
        let objs = vec![
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> /StructTreeRoot 4 0 R >>".to_string(),
            PAGES.to_string(),
            page.to_string(),
            // 4: StructTreeRoot → H1 (no OBJR anywhere)
            "<< /Type /StructTreeRoot /K 5 0 R /ParentTree 6 0 R /ParentTreeNextKey 1 >>".to_string(),
            // 5: H1
            "<< /Type /StructElem /S /H1 /P 4 0 R /Pg 3 0 R /K 0 >>".to_string(),
            // 6: ParentTree
            "<< /Nums [0 7 0 R] >>".to_string(),
            // 7: array
            "<< 5 0 R >>".to_string(),
            // 8,9 unused slots to keep 10 the annot
            "null".to_string(),
            "null".to_string(),
            // 10: Link annotation
            "<< /Type /Annot /Subtype /Link /Rect [0 0 10 10] /P 3 0 R >>".to_string(),
        ];
        let file = PdfFile::parse(build_pdf(
            &objs.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        ))
        .expect("parse");
        let r = validate(&file, Profile::Ua1);
        let rules: Vec<&str> = r.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"annotation-objr"),
            "annotation-objr should fire for a Link with no OBJR: {rules:?}"
        );
    }

    // ---- PDF/UA-2: PDF 2.0 header ----------------------------------------

    /// The same compliant structure that passes UA-1 must still pass UA-2 when
    /// the header is PDF 2.0 (the only UA-2-specific check is the header).
    #[test]
    fn ua2_compliant_pdf20_passes() {
        let file = ua_pdf_v2(
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> \
             /StructTreeRoot 4 0 R >>",
            &[
                // 4: StructTreeRoot
                "<< /Type /StructTreeRoot /K 5 0 R /ParentTree 9 0 R /ParentTreeNextKey 1 >>",
                // 5: Document
                "<< /Type /StructElem /S /Document /P 4 0 R /K [6 0 R 7 0 R] >>",
                // 6: H1
                "<< /Type /StructElem /S /H1 /P 5 0 R /Pg 3 0 R /K 0 >>",
                // 7: P
                "<< /Type /StructElem /S /P /P 5 0 R /Pg 3 0 R /K 1 >>",
                // 8: parent-tree array
                "<< 6 0 R 7 0 R >>",
                // 9: ParentTree number tree
                "<< /Nums [0 8 0 R] >>",
            ],
        );
        let r = validate(&file, Profile::Ua2);
        assert!(
            r.conforms(),
            "expected PDF/UA-2 conformance, got: {:?}",
            r.violations
        );
    }

    /// A PDF 1.7 header is non-conformant for PDF/UA-2 (a PDF 2.0 standard).
    /// The compliant UA-1 structure is reused; only the header differs.
    #[test]
    fn ua2_pdf17_header_is_flagged() {
        let file = ua_pdf(
            "<< /Type /Catalog /Pages 2 0 R /Lang (en) /MarkInfo << /Marked true >> \
             /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R /ParentTree 9 0 R /ParentTreeNextKey 1 >>",
                "<< /Type /StructElem /S /Document /P 4 0 R /K [6 0 R 7 0 R] >>",
                "<< /Type /StructElem /S /H1 /P 5 0 R /Pg 3 0 R /K 0 >>",
                "<< /Type /StructElem /S /P /P 5 0 R /Pg 3 0 R /K 1 >>",
                "<< 6 0 R 7 0 R >>",
                "<< /Nums [0 8 0 R] >>",
            ],
        );
        let r = validate(&file, Profile::Ua2);
        let rules: Vec<&str> = r.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"header-version"),
            "PDF 1.7 header must be flagged for PDF/UA-2: {rules:?}"
        );
        assert!(!r.conforms());
        // The same file under UA-1 has no header constraint → no header-version.
        let r1 = validate(&file, Profile::Ua1);
        let rules1: Vec<&str> = r1.violations.iter().map(|v| v.rule).collect();
        assert!(
            !rules1.contains(&"header-version"),
            "UA-1 must not flag header-version: {rules1:?}"
        );
        assert!(r1.conforms(), "UA-1 should still conform: {rules1:?}");
    }
}
