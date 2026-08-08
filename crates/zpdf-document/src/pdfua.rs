//! PDF/UA-1 conformance validation (ISO 14289-1).
//!
//! A rule engine over the parsed document, mirroring [`crate::pdfa`]: each
//! check inspects one aspect and yields zero or more [`Violation`]s. This
//! covers the high-signal, machine-checkable clauses of PDF/UA-1 (the
//! "Universal Accessibility" conformance):
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
use crate::structure::{is_tagged, parse_struct_tree, StructElem, StructRole, StructTree};

/// The validation profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// ISO 14289-1 (PDF/UA-1).
    Ua1,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Ua1 => "PDF/UA-1",
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

/// Validate `file` against PDF/UA-1.
pub fn validate(file: &PdfFile, profile: Profile) -> ValidationReport {
    let mut v: Vec<Violation> = Vec::new();
    check_tagged(file, &mut v);
    let tree = check_struct_tree(file, &mut v);
    check_lang(file, &mut v);
    if let Some(tree) = &tree {
        check_figures_have_alt(tree, &mut v);
        check_has_heading(tree, &mut v);
        check_roles_standard(tree, &mut v);
    }
    check_page_struct_parents(file, &mut v);
    ValidationReport {
        profile,
        violations: v,
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

    fn ua_pdf(catalog: &str, extra: &[&str]) -> PdfFile {
        let mut objs = vec![catalog, PAGES, PAGE];
        objs.extend_from_slice(extra);
        open(&objs)
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
}
