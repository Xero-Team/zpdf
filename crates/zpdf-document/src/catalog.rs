use std::borrow::Cow;
use std::collections::HashSet;

use tracing::warn;
use zpdf_core::{Error, ObjectId, PdfObject, Result};
use zpdf_parser::PdfFile;

use crate::page::{PdfPage, MAX_PAGE_TREE_DEPTH};

pub struct Catalog {
    pub pages_ref: ObjectId,
    pub page_count: usize,
    page_refs: Vec<ObjectId>,
}

impl Catalog {
    pub fn from_trailer(file: &PdfFile) -> Result<Self> {
        let root_ref = file.trailer.get_ref("Root")?;
        let root = file.resolve(root_ref)?;
        // A lenient resolver may report a dangling /Root as Null instead of an
        // error; either way a document without a catalog is unusable.
        if root.is_null() {
            return Err(Error::InvalidObject(
                0,
                format!("/Root {root_ref} resolves to null"),
            ));
        }
        let root_dict = root.as_dict()?;

        let pages_ref = root_dict.get_ref("Pages")?;

        // /Count is advisory only; the guarded kid walk determines the real
        // page list (broken kids are skipped, cycles and over-deep chains pruned).
        let mut page_refs = Vec::new();
        let mut visited = HashSet::new();
        Self::collect_page_refs(file, pages_ref, &mut page_refs, &mut visited, 0)?;

        if page_refs.is_empty() {
            return Err(Error::InvalidObject(
                0,
                "page tree contains no usable pages".into(),
            ));
        }

        Ok(Self {
            pages_ref,
            page_count: page_refs.len(),
            page_refs,
        })
    }

    fn collect_page_refs(
        file: &PdfFile,
        node_id: ObjectId,
        refs: &mut Vec<ObjectId>,
        visited: &mut HashSet<ObjectId>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_PAGE_TREE_DEPTH {
            warn!("page tree deeper than {MAX_PAGE_TREE_DEPTH} at {node_id}; pruning subtree");
            return Ok(());
        }
        if !visited.insert(node_id) {
            warn!("page tree cycle: node {node_id} already visited; pruning");
            return Ok(());
        }

        let node = match file.resolve(node_id) {
            Ok(PdfObject::Null) => {
                warn!("page tree node {node_id} resolves to null; skipping");
                return Ok(());
            }
            Ok(obj) => obj,
            Err(e) => {
                warn!("failed to resolve page tree node {node_id}: {e}; skipping");
                return Ok(());
            }
        };
        let Ok(dict) = node.as_dict() else {
            warn!(
                "page tree node {node_id} is {}, expected Dict; skipping",
                node.type_name()
            );
            return Ok(());
        };

        // /Type is formally required but missing or wrong in real-world files;
        // fall back on the presence of /Kids to tell interior nodes from leaves.
        let is_pages = match dict.get_name("Type") {
            Ok("Pages") => true,
            Ok("Page") => false,
            _ => dict.get("Kids").is_some(),
        };

        if is_pages {
            // /Kids may itself be an indirect ref to the array.
            let kids: Cow<'_, [PdfObject]> = match dict.get("Kids") {
                Some(PdfObject::Array(a)) => Cow::Borrowed(a.as_slice()),
                Some(PdfObject::Ref(r)) => match file.resolve(*r) {
                    Ok(PdfObject::Array(a)) => Cow::Owned(a),
                    _ => {
                        warn!("pages node {node_id}: /Kids ref {r} is not an array; skipping");
                        return Ok(());
                    }
                },
                _ => {
                    warn!("pages node {node_id} has no /Kids array; skipping");
                    return Ok(());
                }
            };
            for kid in kids.iter() {
                match kid {
                    PdfObject::Ref(r) => {
                        Self::collect_page_refs(file, *r, refs, visited, depth + 1)?;
                    }
                    PdfObject::Null => {
                        warn!("pages node {node_id}: null kid; skipping");
                    }
                    other => {
                        warn!(
                            "pages node {node_id}: kid is {}, expected Ref; skipping",
                            other.type_name()
                        );
                    }
                }
            }
        } else {
            refs.push(node_id);
        }
        Ok(())
    }

    pub fn get_page(&self, file: &PdfFile, index: usize) -> Result<PdfPage> {
        let page_ref =
            self.page_refs.get(index).copied().ok_or_else(|| {
                Error::InvalidObject(0, format!("page index {index} out of range"))
            })?;

        PdfPage::from_object(file, page_ref)
    }
}

#[cfg(test)]
mod tests {
    use crate::page::MAX_PAGE_TREE_DEPTH;
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    #[test]
    fn kids_cycle_is_pruned() {
        // The pages node lists itself as a kid; the walk must terminate.
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 2 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        ]))
        .expect("open");
        assert_eq!(doc.page_count(), 1);
    }

    #[test]
    fn dangling_and_null_kids_are_skipped() {
        // 99 0 R is dangling (skipped whether resolve errors or returns Null);
        // the literal null kid is skipped outright.
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [99 0 R 3 0 R null] /Count 3 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        ]))
        .expect("open");
        assert_eq!(doc.page_count(), 1);
        assert!(doc.page(0).is_ok());
    }

    #[test]
    fn missing_type_nodes_tolerated() {
        // Neither tree node carries /Type; /Kids presence tells interior from
        // leaf, and inheritance still works through the untyped interior node.
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 200] >>",
            "<< /Parent 2 0 R >>",
        ]))
        .expect("open");
        assert_eq!(doc.page_count(), 1);
        let page = doc.page(0).expect("page");
        assert_eq!(page.media_box.width(), 200.0);
    }

    #[test]
    fn empty_page_tree_is_an_error() {
        assert!(PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [] /Count 0 >>",
        ]))
        .is_err());
    }

    #[test]
    fn null_root_is_a_hard_error() {
        // Object 1 (the /Root target) is the literal null object.
        assert!(PdfDocument::open(build_pdf(&["null"])).is_err());
    }

    #[test]
    fn overly_deep_page_tree_is_pruned() {
        // A single-kid Pages chain deeper than the guard: opening must
        // terminate, and with the only leaf pruned the tree comes up empty.
        let mut objects: Vec<String> = vec!["<< /Type /Catalog /Pages 2 0 R >>".into()];
        let chain = MAX_PAGE_TREE_DEPTH + 10;
        for i in 0..chain {
            objects.push(format!("<< /Type /Pages /Kids [{} 0 R] /Count 1 >>", i + 3));
        }
        objects.push("<< /Type /Page /MediaBox [0 0 10 10] >>".into());
        let refs: Vec<&str> = objects.iter().map(|s| s.as_str()).collect();
        assert!(PdfDocument::open(build_pdf(&refs)).is_err());
    }
}
