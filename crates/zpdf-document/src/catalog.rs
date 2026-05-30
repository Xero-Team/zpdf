use zpdf_core::{Error, ObjectId, PdfObject, Result};
use zpdf_parser::PdfFile;

use crate::page::PdfPage;

pub struct Catalog {
    pub pages_ref: ObjectId,
    pub page_count: usize,
    page_refs: Vec<ObjectId>,
}

impl Catalog {
    pub fn from_trailer(file: &PdfFile) -> Result<Self> {
        let root_ref = file.trailer.get_ref("Root")?;
        let root = file.resolve(root_ref)?;
        let root_dict = root.as_dict()?;

        let pages_ref = root_dict.get_ref("Pages")?;
        let pages = file.resolve(pages_ref)?;
        let pages_dict = pages.as_dict()?;

        let count = pages_dict.get_i64("Count")? as usize;

        let mut page_refs = Vec::with_capacity(count);
        Self::collect_page_refs(file, pages_ref, &mut page_refs)?;

        Ok(Self {
            pages_ref,
            page_count: count,
            page_refs,
        })
    }

    fn collect_page_refs(
        file: &PdfFile,
        node_id: ObjectId,
        refs: &mut Vec<ObjectId>,
    ) -> Result<()> {
        let node = file.resolve(node_id)?;
        let dict = node.as_dict()?;

        let type_name = dict.get_name("Type")?;
        match type_name {
            "Pages" => {
                let kids = dict.get_array("Kids")?;
                for kid in kids {
                    let kid_ref = kid.as_ref()?;
                    Self::collect_page_refs(file, kid_ref, refs)?;
                }
            }
            "Page" => {
                refs.push(node_id);
            }
            _ => {
                return Err(Error::TypeMismatch {
                    expected: "Pages or Page",
                    actual: "other",
                });
            }
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
