use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use tracing::warn;
use zpdf_core::{ObjectId, PdfDict, PdfObject, Rect, Result};
use zpdf_parser::PdfFile;

/// Hard cap on page-tree walks (`/Parent` chains and `/Kids` recursion) — far
/// deeper than any sane document, it bounds malformed or adversarial trees in
/// concert with the visited-set cycle checks.
pub(crate) const MAX_PAGE_TREE_DEPTH: usize = 64;

#[derive(Debug)]
pub struct PdfPage {
    pub id: ObjectId,
    pub media_box: Rect,
    pub crop_box: Rect,
    pub rotate: i32,
    pub resources: ResourceDict,
    pub contents: Vec<ObjectId>,
    /// Annotation object ids from `/Annots`, parsed but not yet rendered.
    pub annots: Vec<ObjectId>,
}

#[derive(Debug, Default)]
pub struct ResourceDict {
    pub fonts: HashMap<String, ObjectId>,
    pub xobjects: HashMap<String, ObjectId>,
    pub ext_g_state: HashMap<String, ObjectId>,
    pub ext_g_state_inline: HashMap<String, zpdf_core::PdfDict>,
    pub color_spaces: HashMap<String, ObjectId>,
    /// Colorspace resources whose value is a direct array/name rather than a
    /// reference (common from Quartz and Ghostscript).
    pub color_spaces_inline: HashMap<String, PdfObject>,
    pub patterns: HashMap<String, ObjectId>,
    pub shadings: HashMap<String, ObjectId>,
    pub shadings_inline: HashMap<String, PdfObject>,
}

impl PdfPage {
    pub fn from_object(file: &PdfFile, page_id: ObjectId) -> Result<Self> {
        let obj = file.resolve(page_id)?;
        let dict = obj.as_dict()?;

        // MediaBox, CropBox, Rotate and Resources are all inheritable page
        // attributes (PDF 32000-1 Table 31): one guarded walk up /Parent
        // gathers whichever values the leaf doesn't carry itself.
        let inherited = InheritedAttrs::gather(file, dict);

        let media_box = inherited
            .media_box
            .ok_or_else(|| zpdf_core::Error::MissingKey("MediaBox".into()))?;
        let crop_box = inherited.crop_box.unwrap_or(media_box);
        let rotate = inherited.rotate.unwrap_or(0);
        let resources = inherited.resources.unwrap_or_default();

        let contents = Self::collect_content_refs(file, dict.get("Contents"));
        let annots = Self::collect_annot_refs(file, dict.get("Annots"));

        Ok(Self {
            id: page_id,
            media_box,
            crop_box,
            rotate,
            resources,
            contents,
            annots,
        })
    }

    /// Collect the page's content-stream object ids from `/Contents`, which may
    /// be: a single stream ref; a direct array of stream refs; or — as some
    /// scanners emit — an indirect ref *to* an array of stream refs (double
    /// indirection). The latter is resolved one level so the array is flattened
    /// rather than mistaken for a single (non-stream) object.
    fn collect_content_refs(file: &PdfFile, contents: Option<&PdfObject>) -> Vec<ObjectId> {
        fn refs_from_array(arr: &[PdfObject]) -> Vec<ObjectId> {
            arr.iter()
                .filter_map(|o| match o {
                    PdfObject::Ref(r) => Some(*r),
                    _ => None,
                })
                .collect()
        }
        match contents {
            Some(PdfObject::Array(arr)) => refs_from_array(arr),
            Some(PdfObject::Ref(r)) => match file.resolve(*r) {
                // Ref → array of stream refs: flatten it.
                Ok(PdfObject::Array(arr)) => refs_from_array(&arr),
                // Ref → a single content stream: keep the ref itself.
                Ok(PdfObject::Stream(_)) => vec![*r],
                // Anything else (incl. resolve failure): treat as the lone ref so
                // a later resolve attempt surfaces the real error.
                _ => vec![*r],
            },
            _ => vec![],
        }
    }

    /// Collect annotation object ids from `/Annots` (a direct array or a ref
    /// to an array). Parse-only plumbing: appearance streams are not rendered.
    fn collect_annot_refs(file: &PdfFile, annots: Option<&PdfObject>) -> Vec<ObjectId> {
        fn refs_from_array(arr: &[PdfObject]) -> Vec<ObjectId> {
            arr.iter()
                .filter_map(|o| match o {
                    PdfObject::Ref(r) => Some(*r),
                    _ => None,
                })
                .collect()
        }
        match annots {
            Some(PdfObject::Array(arr)) => refs_from_array(arr),
            Some(PdfObject::Ref(r)) => match file.resolve(*r) {
                Ok(PdfObject::Array(arr)) => refs_from_array(&arr),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    pub fn width(&self) -> f64 {
        self.media_box.width()
    }

    pub fn height(&self) -> f64 {
        self.media_box.height()
    }

    /// The rectangle the page is rendered into: `/CropBox` intersected with
    /// `/MediaBox`. Per spec a CropBox extending beyond the MediaBox is
    /// clamped to it; an empty or non-overlapping CropBox falls back to the
    /// full MediaBox.
    pub fn effective_box(&self) -> Rect {
        let media = self.media_box.normalize();
        let crop = self.crop_box.normalize();
        let inter = Rect::new(
            crop.x0.max(media.x0),
            crop.y0.max(media.y0),
            crop.x1.min(media.x1),
            crop.y1.min(media.y1),
        );
        if inter.x1 > inter.x0 && inter.y1 > inter.y0 {
            inter
        } else {
            media
        }
    }
}

/// Inheritable page attributes (PDF 32000-1 Table 31), filled in leaf-first
/// while walking up the `/Parent` chain with cycle and depth guards.
#[derive(Default)]
struct InheritedAttrs {
    media_box: Option<Rect>,
    crop_box: Option<Rect>,
    rotate: Option<i32>,
    resources: Option<ResourceDict>,
}

impl InheritedAttrs {
    fn is_complete(&self) -> bool {
        self.media_box.is_some()
            && self.crop_box.is_some()
            && self.rotate.is_some()
            && self.resources.is_some()
    }

    fn gather(file: &PdfFile, leaf: &PdfDict) -> Self {
        let mut attrs = Self::default();
        let mut visited: HashSet<ObjectId> = HashSet::new();
        let mut current: Cow<'_, PdfDict> = Cow::Borrowed(leaf);
        let mut depth = 0usize;

        loop {
            attrs.absorb(file, &current);
            if attrs.is_complete() {
                break;
            }
            let parent_ref = match current.get("Parent") {
                Some(PdfObject::Ref(r)) => *r,
                _ => break,
            };
            depth += 1;
            if depth > MAX_PAGE_TREE_DEPTH {
                warn!("page-tree /Parent chain deeper than {MAX_PAGE_TREE_DEPTH}; stopping inheritance walk");
                break;
            }
            if !visited.insert(parent_ref) {
                warn!("page-tree /Parent cycle at {parent_ref}; stopping inheritance walk");
                break;
            }
            match file.resolve(parent_ref) {
                Ok(PdfObject::Dict(d)) => current = Cow::Owned(d),
                Ok(PdfObject::Null) => {
                    warn!(
                        "page-tree parent {parent_ref} resolves to null; stopping inheritance walk"
                    );
                    break;
                }
                Ok(other) => {
                    warn!(
                        "page-tree parent {parent_ref} is {}, expected Dict; stopping inheritance walk",
                        other.type_name()
                    );
                    break;
                }
                Err(e) => {
                    warn!("failed to resolve page-tree parent {parent_ref}: {e}");
                    break;
                }
            }
        }
        attrs
    }

    /// Pick up any attribute the walk hasn't found yet from `dict`. Values
    /// closer to the leaf win, so only `None` slots are filled.
    fn absorb(&mut self, file: &PdfFile, dict: &PdfDict) {
        if self.media_box.is_none() {
            self.media_box = resolve_rect(file, dict, "MediaBox");
        }
        if self.crop_box.is_none() {
            self.crop_box = resolve_rect(file, dict, "CropBox");
        }
        if self.rotate.is_none() {
            self.rotate = resolve_i64(file, dict.get("Rotate")).map(|n| n as i32);
        }
        if self.resources.is_none() {
            if let Some(d) = resolve_sub_dict(dict, "Resources", file) {
                match parse_resource_dict(&d, file) {
                    Ok(r) => self.resources = Some(r),
                    Err(e) => warn!("failed to parse /Resources: {e}"),
                }
            }
        }
    }
}

/// Read a rectangle value that may be a direct array, an indirect ref to an
/// array, or an array whose elements are themselves indirect number refs.
fn resolve_rect(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<Rect> {
    let arr: Cow<'_, [PdfObject]> = match dict.get(key)? {
        PdfObject::Array(a) => Cow::Borrowed(a.as_slice()),
        PdfObject::Ref(r) => match file.resolve(*r) {
            Ok(PdfObject::Array(a)) => Cow::Owned(a),
            Ok(other) => {
                warn!(
                    "/{key} ref {r} resolved to {}, expected Array",
                    other.type_name()
                );
                return None;
            }
            Err(e) => {
                warn!("failed to resolve /{key} ref {r}: {e}");
                return None;
            }
        },
        _ => return None,
    };
    if arr.len() != 4 {
        warn!("/{key} array has {} elements, expected 4", arr.len());
        return None;
    }
    let mut v = [0f64; 4];
    for (slot, obj) in v.iter_mut().zip(arr.iter()) {
        *slot = match obj {
            PdfObject::Ref(r) => file.resolve(*r).ok()?.as_f64().ok()?,
            other => other.as_f64().ok()?,
        };
    }
    Some(Rect::new(v[0], v[1], v[2], v[3]))
}

/// Read an integer value that may be direct or an indirect ref.
fn resolve_i64(file: &PdfFile, value: Option<&PdfObject>) -> Option<i64> {
    match value? {
        PdfObject::Integer(n) => Some(*n),
        PdfObject::Real(r) => Some(*r as i64),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Integer(n) => Some(n),
            PdfObject::Real(r) => Some(r as i64),
            _ => None,
        },
        _ => None,
    }
}

fn resolve_sub_dict<'a>(
    dict: &'a zpdf_core::PdfDict,
    key: &str,
    file: &'a PdfFile,
) -> Option<std::borrow::Cow<'a, zpdf_core::PdfDict>> {
    match dict.get(key) {
        Some(PdfObject::Dict(d)) => Some(std::borrow::Cow::Borrowed(d)),
        Some(PdfObject::Ref(r)) => file.resolve(*r).ok().and_then(|o| match o {
            PdfObject::Dict(d) => Some(std::borrow::Cow::Owned(d)),
            _ => None,
        }),
        _ => None,
    }
}

pub fn parse_resource_dict(dict: &zpdf_core::PdfDict, file: &PdfFile) -> Result<ResourceDict> {
    let mut res = ResourceDict::default();

    if let Some(fonts) = resolve_sub_dict(dict, "Font", file) {
        for (name, obj) in &fonts.0 {
            if let PdfObject::Ref(r) = obj {
                res.fonts.insert(name.0.clone(), *r);
            }
        }
    }

    if let Some(xobjects) = resolve_sub_dict(dict, "XObject", file) {
        for (name, obj) in &xobjects.0 {
            if let PdfObject::Ref(r) = obj {
                res.xobjects.insert(name.0.clone(), *r);
            }
        }
    }

    if let Some(gs) = resolve_sub_dict(dict, "ExtGState", file) {
        for (name, obj) in &gs.0 {
            match obj {
                PdfObject::Ref(r) => {
                    res.ext_g_state.insert(name.0.clone(), *r);
                }
                PdfObject::Dict(d) => {
                    res.ext_g_state_inline.insert(name.0.clone(), d.clone());
                }
                _ => {}
            }
        }
    }

    if let Some(cs) = resolve_sub_dict(dict, "ColorSpace", file) {
        for (name, obj) in &cs.0 {
            match obj {
                PdfObject::Ref(r) => {
                    res.color_spaces.insert(name.0.clone(), *r);
                }
                other @ (PdfObject::Array(_) | PdfObject::Name(_)) => {
                    res.color_spaces_inline
                        .insert(name.0.clone(), other.clone());
                }
                _ => {}
            }
        }
    }

    if let Some(pat) = resolve_sub_dict(dict, "Pattern", file) {
        for (name, obj) in &pat.0 {
            if let PdfObject::Ref(r) = obj {
                res.patterns.insert(name.0.clone(), *r);
            }
        }
    }

    if let Some(sh) = resolve_sub_dict(dict, "Shading", file) {
        for (name, obj) in &sh.0 {
            match obj {
                PdfObject::Ref(r) => {
                    res.shadings.insert(name.0.clone(), *r);
                }
                other @ PdfObject::Dict(_) => {
                    res.shadings_inline.insert(name.0.clone(), other.clone());
                }
                _ => {}
            }
        }
    }

    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    /// Open a synthetic PDF and return its first page.
    fn page0(objects: &[&str]) -> PdfPage {
        let doc = PdfDocument::open(build_pdf(objects)).expect("open");
        doc.page(0).expect("page")
    }

    #[test]
    fn rotate_and_resources_inherited_from_pages_node() {
        let page = page0(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] /Rotate 90 /Resources << /Font << /F1 4 0 R >> >> >>",
            "<< /Type /Page /Parent 2 0 R >>",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ]);
        assert_eq!(page.rotate, 90);
        assert_eq!(page.media_box, Rect::new(0.0, 0.0, 612.0, 792.0));
        assert_eq!(page.resources.fonts.get("F1"), Some(&ObjectId(4, 0)));
    }

    #[test]
    fn leaf_attributes_override_inherited() {
        let page = page0(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] /Rotate 90 /Resources << /Font << /F1 4 0 R >> >> >>",
            "<< /Type /Page /Parent 2 0 R /Rotate 180 /Resources << /Font << /F2 4 0 R >> >> >>",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ]);
        assert_eq!(page.rotate, 180);
        assert!(page.resources.fonts.contains_key("F2"));
        // The leaf's own /Resources replaces (not merges with) the parent's.
        assert!(!page.resources.fonts.contains_key("F1"));
    }

    #[test]
    fn indirect_media_and_crop_boxes_resolve() {
        let page = page0(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox 4 0 R /CropBox [10 10 5 0 R 200] >>",
            "[0 0 300 400]",
            "100",
        ]);
        assert_eq!(page.media_box, Rect::new(0.0, 0.0, 300.0, 400.0));
        assert_eq!(page.crop_box, Rect::new(10.0, 10.0, 100.0, 200.0));
    }

    #[test]
    fn parent_cycle_terminates_and_keeps_found_values() {
        // Nodes 2 and 3 name each other as /Parent; the walk must terminate
        // and still pick up the MediaBox found before the cycle closes.
        let page = page0(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /Parent 3 0 R /MediaBox [0 0 100 100] >>",
            "<< /Type /Page /Parent 2 0 R >>",
        ]);
        assert_eq!(page.media_box, Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(page.rotate, 0);
    }

    #[test]
    fn annots_refs_collected() {
        let page = page0(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Annots [4 0 R 5 0 R] >>",
            "<< /Type /Annot /Subtype /Link >>",
            "<< /Type /Annot /Subtype /Square >>",
        ]);
        assert_eq!(page.annots, vec![ObjectId(4, 0), ObjectId(5, 0)]);
    }

    fn page_with_boxes(media: Rect, crop: Rect) -> PdfPage {
        PdfPage {
            id: ObjectId(1, 0),
            media_box: media,
            crop_box: crop,
            rotate: 0,
            resources: ResourceDict::default(),
            contents: vec![],
            annots: vec![],
        }
    }

    #[test]
    fn effective_box_intersects_crop_with_media() {
        let media = Rect::new(0.0, 0.0, 612.0, 792.0);
        // CropBox inside MediaBox: used as-is.
        let p = page_with_boxes(media, Rect::new(10.0, 20.0, 500.0, 700.0));
        assert_eq!(p.effective_box(), Rect::new(10.0, 20.0, 500.0, 700.0));
        // CropBox sticking out on every side: clamped to the MediaBox.
        let p = page_with_boxes(media, Rect::new(-50.0, -50.0, 700.0, 800.0));
        assert_eq!(p.effective_box(), media);
        // Partial overlap: the intersection.
        let p = page_with_boxes(media, Rect::new(300.0, 400.0, 900.0, 900.0));
        assert_eq!(p.effective_box(), Rect::new(300.0, 400.0, 612.0, 792.0));
    }

    #[test]
    fn effective_box_falls_back_to_media_box() {
        let media = Rect::new(0.0, 0.0, 612.0, 792.0);
        // Disjoint CropBox.
        let p = page_with_boxes(media, Rect::new(1000.0, 1000.0, 1100.0, 1100.0));
        assert_eq!(p.effective_box(), media);
        // Degenerate (zero-area) CropBox.
        let p = page_with_boxes(media, Rect::new(100.0, 100.0, 100.0, 100.0));
        assert_eq!(p.effective_box(), media);
        // Default: CropBox == MediaBox.
        let p = page_with_boxes(media, media);
        assert_eq!(p.effective_box(), media);
    }

    #[test]
    fn effective_box_normalizes_inverted_crop() {
        let media = Rect::new(0.0, 0.0, 612.0, 792.0);
        let p = page_with_boxes(media, Rect::new(500.0, 700.0, 10.0, 20.0));
        assert_eq!(p.effective_box(), Rect::new(10.0, 20.0, 500.0, 700.0));
    }
}
