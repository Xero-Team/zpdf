//! Document outline / bookmarks (ISO 32000-1 §12.3.3). The catalog's
//! `/Outlines` dictionary roots a tree of outline items, each a `/Title` plus a
//! navigation target — a destination (`/Dest`) or an action (`/A`, typically a
//! go-to or a URI). Items are linked as a doubly-linked sibling list (`/First`,
//! `/Last`, `/Next`, `/Prev`) with `/First`/`/Count` descending into children.
//!
//! This reads the tree into a nested [`OutlineItem`] structure, resolving each
//! item's target through [`crate::destinations`]. Bounded by depth, a visited
//! set, and a global item cap so a malformed/cyclic tree cannot loop.

use std::collections::{HashMap, HashSet};

use zpdf_core::{ObjectId, PdfDict, PdfObject};
use zpdf_parser::PdfFile;

use crate::destinations::{collect_named_dests, resolve_explicit_with, Destination};
use crate::forms::pdf_string_to_unicode;
use crate::obj_util::{catalog_dict, resolve_dict, resolve_name, resolve_number, text};
use crate::Catalog;

/// Maximum nesting depth of the outline tree before a subtree is pruned.
const MAX_OUTLINE_DEPTH: usize = 64;
/// Global cap on outline items collected from one document.
const MAX_OUTLINE_ITEMS: usize = 65_536;

/// One bookmark: a title, an optional navigation target, and nested children.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineItem {
    /// `/Title` — the bookmark label (text string; UTF-16BE/PDFDoc decoded).
    pub title: String,
    /// The resolved navigation destination (`/Dest`, or a go-to action's `/D`),
    /// when this item carries one.
    pub dest: Option<Destination>,
    /// A URI target (`/A` with `/S /URI`), or a remote go-to file path
    /// (`/S /GoToR` `/F`), when this item links outside the page model.
    pub uri: Option<String>,
    /// `/Count` > 0: the item is *open* (its children shown by default).
    pub open: bool,
    /// Nested child bookmarks (from `/First` … `/Next`).
    pub children: Vec<OutlineItem>,
}

/// Parse the document outline (bookmarks). Empty when the document has none.
pub fn parse_outlines(file: &PdfFile, catalog: &Catalog) -> Vec<OutlineItem> {
    let Some(root) = catalog_dict(file) else {
        return Vec::new();
    };
    let Some(outlines) = resolve_dict(file, root.get("Outlines")) else {
        return Vec::new();
    };

    let mut visited = HashSet::new();
    // Seed the cycle guard with the outline-root reference, so a malicious item
    // whose /Next or /First points back at the root cannot spawn a spurious pass
    // (parity with embedded_files / destinations tree-root seeding).
    if let Some(PdfObject::Ref(id)) = root.get("Outlines") {
        visited.insert(*id);
    }
    // Flatten the named-destination registries once, so each bookmark's named
    // destination resolves in O(1) against this map rather than re-walking the
    // name tree per item (which a crafted file could turn into a DoS).
    let named = collect_named_dests(file);
    let mut walk = OutlineWalk {
        file,
        catalog,
        named: &named,
        visited,
        count: 0,
    };

    // The outline root's /First begins the top-level sibling chain.
    let mut out = Vec::new();
    if let Some(first_ref) = outlines.get("First").and_then(as_ref) {
        walk.walk_siblings(first_ref, &mut out, 0);
    }
    out
}

/// Shared state for one outline traversal: the ambient object graph plus the
/// cross-tree cycle guard and item budget. Collected into a context so the
/// recursive walk methods stay legible (and avoid a long argument list).
struct OutlineWalk<'a> {
    file: &'a PdfFile,
    catalog: &'a Catalog,
    /// Pre-collected named destinations (see [`collect_named_dests`]).
    named: &'a HashMap<Vec<u8>, PdfObject>,
    /// Every outline item reference seen so far — a `/Next`/`/First` back-edge
    /// to any of them terminates that chain.
    visited: HashSet<ObjectId>,
    /// Total items collected, capped at [`MAX_OUTLINE_ITEMS`].
    count: usize,
}

impl OutlineWalk<'_> {
    /// Walk a sibling chain (`item` → `/Next` → …), appending each item.
    fn walk_siblings(&mut self, mut item_ref: ObjectId, out: &mut Vec<OutlineItem>, depth: usize) {
        loop {
            if depth > MAX_OUTLINE_DEPTH || self.count >= MAX_OUTLINE_ITEMS {
                return;
            }
            // Cycle guard: a /Next or /First that points back to a seen item stops.
            if !self.visited.insert(item_ref) {
                return;
            }
            self.count += 1;

            let Some(dict) = self
                .file
                .resolve(item_ref)
                .ok()
                .and_then(|o| o.as_dict().ok().cloned())
            else {
                return;
            };

            let item = self.build_item(&dict, depth);
            out.push(item);

            match dict.get("Next").and_then(as_ref) {
                Some(next) => item_ref = next,
                None => return,
            }
        }
    }

    /// Build one [`OutlineItem`] from its dictionary, recursing into `/First` for
    /// children and resolving its `/Dest` or `/A` target.
    fn build_item(&mut self, dict: &PdfDict, depth: usize) -> OutlineItem {
        let title = text(self.file, dict, "Title").unwrap_or_default();
        let (dest, uri) = self.resolve_target(dict);

        // /Count > 0 means the item is displayed open (children visible). The
        // magnitude is the visible-descendant count; only the sign matters here.
        // Read it through the resolving numeric accessor so an indirect or Real
        // /Count is honoured (matching the module's other numeric reads).
        let open = resolve_number(self.file, dict.get("Count")).is_some_and(|c| c > 0.0);

        let mut children = Vec::new();
        if let Some(first) = dict.get("First").and_then(as_ref) {
            self.walk_siblings(first, &mut children, depth + 1);
        }

        OutlineItem {
            title,
            dest,
            uri,
            open,
            children,
        }
    }

    /// Resolve an outline item's navigation target: `/Dest` directly, or `/A`
    /// (action). Returns `(destination, uri)`.
    fn resolve_target(&self, dict: &PdfDict) -> (Option<Destination>, Option<String>) {
        // A direct /Dest takes precedence (a name, string, or explicit array).
        if let Some(dest_obj) = dict.get("Dest") {
            if let Some(d) = resolve_explicit_with(self.file, self.catalog, dest_obj, self.named) {
                return (Some(d), None);
            }
        }

        // Otherwise an action /A: go-to (a destination) or URI (a hyperlink).
        if let Some(action) = resolve_dict(self.file, dict.get("A")) {
            match resolve_name(self.file, action.get("S")).as_deref() {
                Some("GoTo") => {
                    if let Some(d) = action
                        .get("D")
                        .and_then(|d| resolve_explicit_with(self.file, self.catalog, d, self.named))
                    {
                        return (Some(d), None);
                    }
                }
                Some("URI") => {
                    if let Some(uri) = uri_string(self.file, &action) {
                        return (None, Some(uri));
                    }
                }
                Some("GoToR") => {
                    // Remote go-to: record the destination *file* as the target.
                    if let Some(name) = remote_file_name(self.file, &action) {
                        return (None, Some(name));
                    }
                }
                _ => {}
            }
        }

        (None, None)
    }
}

/// The `/URI` of a URI action — a byte string (often ASCII); decode leniently.
fn uri_string(file: &PdfFile, action: &PdfDict) -> Option<String> {
    let value = match action.get("URI")? {
        PdfObject::String(s) => return Some(decode_ascii(s.as_bytes())),
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        _ => return None,
    };
    match value {
        PdfObject::String(s) => Some(decode_ascii(s.as_bytes())),
        _ => None,
    }
}

/// The target file name of a remote go-to action (`/F` — a string or a file
/// specification dictionary's `/F`/`/UF`). A bare `/F` string is decoded
/// BOM-aware (UTF-16BE when it carries the `FE FF` BOM), matching the
/// filespec-dict path (which goes through [`text`]) and the `embedded_files`
/// reader — so the same byte string decodes the same way whichever shape it
/// arrives in.
fn remote_file_name(file: &PdfFile, action: &PdfDict) -> Option<String> {
    match action.get("F")? {
        PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
        PdfObject::Dict(d) => filespec_name(file, d),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
            PdfObject::Dict(d) => filespec_name(file, &d),
            _ => None,
        },
        _ => None,
    }
}

/// `/UF` (preferred) or `/F` off a file-specification dictionary.
fn filespec_name(file: &PdfFile, dict: &PdfDict) -> Option<String> {
    text(file, dict, "UF").or_else(|| text(file, dict, "F"))
}

/// Decode a (predominantly ASCII) URI/path string, keeping bytes as Latin-1 so
/// no byte is lost. URIs are 7-bit ASCII per spec; percent-encoding is left raw.
fn decode_ascii(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// An object that is (or resolves to) an indirect reference's id.
fn as_ref(obj: &PdfObject) -> Option<ObjectId> {
    match obj {
        PdfObject::Ref(r) => Some(*r),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::destinations::DestView;
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    fn open(objects: &[&str]) -> PdfDocument {
        PdfDocument::open(build_pdf(objects)).expect("open pdf")
    }

    const PAGES2: &str = "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>";
    const PAGE_A: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";
    const PAGE_B: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";

    #[test]
    fn no_outlines_is_empty() {
        let doc = open(&["<< /Type /Catalog /Pages 2 0 R >>", PAGES2, PAGE_A, PAGE_B]);
        assert!(doc.outline().is_empty());
    }

    #[test]
    fn single_item_with_explicit_dest() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Chapter 1) /Parent 5 0 R /Dest [4 0 R /Fit] >>",
        ]);
        let outline = doc.outline();
        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].title, "Chapter 1");
        let dest = outline[0].dest.as_ref().expect("dest");
        assert_eq!(dest.page, Some(1));
        assert_eq!(dest.view, DestView::Fit);
        assert!(outline[0].children.is_empty());
    }

    #[test]
    fn sibling_chain_in_order() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 8 0 R /Count 3 >>",
            "<< /Title (One)   /Parent 5 0 R /Next 7 0 R >>",
            "<< /Title (Two)   /Parent 5 0 R /Prev 6 0 R /Next 8 0 R >>",
            "<< /Title (Three) /Parent 5 0 R /Prev 7 0 R >>",
        ]);
        let titles: Vec<_> = doc.outline().into_iter().map(|i| i.title).collect();
        assert_eq!(titles, ["One", "Two", "Three"]);
    }

    #[test]
    fn nested_children_and_open_flag() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 2 >>",
            "<< /Title (Parent) /Parent 5 0 R /First 7 0 R /Last 7 0 R /Count 1 >>",
            "<< /Title (Child) /Parent 6 0 R >>",
        ]);
        let outline = doc.outline();
        assert_eq!(outline.len(), 1);
        assert!(outline[0].open, "/Count 1 (> 0) means open");
        assert_eq!(outline[0].children.len(), 1);
        assert_eq!(outline[0].children[0].title, "Child");
    }

    #[test]
    fn closed_item_negative_count() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Collapsed) /Parent 5 0 R /First 7 0 R /Last 7 0 R /Count -1 >>",
            "<< /Title (Hidden child) /Parent 6 0 R >>",
        ]);
        let outline = doc.outline();
        assert!(!outline[0].open, "/Count -1 (< 0) means closed");
        // Children are still parsed (a viewer may expand them); only `open` differs.
        assert_eq!(outline[0].children.len(), 1);
    }

    #[test]
    fn open_flag_honors_indirect_and_real_count() {
        // /Count as an indirect ref (legal) and as a Real (lax) must still set open.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Indirect) /Parent 5 0 R /First 7 0 R /Last 7 0 R /Count 8 0 R >>",
            "<< /Title (Child) /Parent 6 0 R >>",
            "2", // object 8: the indirect /Count value
        ]);
        assert!(doc.outline()[0].open, "indirect /Count > 0 means open");

        let doc_real = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Real) /Parent 5 0 R /First 7 0 R /Last 7 0 R /Count 3.0 >>",
            "<< /Title (Child) /Parent 6 0 R >>",
        ]);
        assert!(doc_real.outline()[0].open, "Real /Count > 0 means open");
    }

    #[test]
    fn item_next_pointing_to_root_makes_no_spurious_item() {
        // The top-level item's /Next points back at the /Outlines root object;
        // the root is pre-seeded into the visited set, so no bogus item appears.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Only) /Parent 5 0 R /Next 5 0 R >>",
        ]);
        let titles: Vec<_> = doc.outline().into_iter().map(|i| i.title).collect();
        assert_eq!(titles, ["Only"], "root back-edge yields no spurious item");
    }

    #[test]
    fn uri_action_captured() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Website) /Parent 5 0 R /A << /S /URI /URI (https://example.com) >> >>",
        ]);
        let outline = doc.outline();
        assert_eq!(outline[0].uri.as_deref(), Some("https://example.com"));
        assert!(outline[0].dest.is_none());
    }

    #[test]
    fn goto_action_dest_resolved() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Go) /Parent 5 0 R /A << /S /GoTo /D [3 0 R /XYZ null 700 null] >> >>",
        ]);
        let dest = doc.outline()[0].dest.clone().expect("dest");
        assert_eq!(dest.page, Some(0));
        assert_eq!(
            dest.view,
            DestView::Xyz {
                left: None,
                top: Some(700.0),
                zoom: None,
            }
        );
    }

    #[test]
    fn gotor_remote_file_name_captured() {
        // A GoToR action records the destination *file* as the item's uri.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Manual) /Parent 5 0 R /A << /S /GoToR /F (manual.pdf) >> >>",
        ]);
        let item = &doc.outline()[0];
        assert_eq!(item.uri.as_deref(), Some("manual.pdf"));
        assert!(item.dest.is_none());
    }

    #[test]
    fn gotor_filespec_prefers_uf() {
        // A /F file-specification dictionary: /UF (Unicode) wins over /F.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Doc) /Parent 5 0 R /A << /S /GoToR /F << /F (legacy.txt) /UF (unicode.txt) >> >> >>",
        ]);
        assert_eq!(doc.outline()[0].uri.as_deref(), Some("unicode.txt"));
    }

    #[test]
    fn gotor_utf16be_filename_decoded() {
        // A bare /F carrying a UTF-16BE BOM decodes BOM-aware (consistent with
        // the filespec path), not as raw Latin-1. <FEFF 0066 0069> = "fi".
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Doc) /Parent 5 0 R /A << /S /GoToR /F <FEFF00660069> >> >>",
        ]);
        assert_eq!(doc.outline()[0].uri.as_deref(), Some("fi"));
    }

    #[test]
    fn named_dest_via_legacy_root_dests() {
        // Outline item naming a destination registered in the legacy /Root /Dests
        // dict — resolved through the once-collected named-destination map.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R /Dests 7 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Legacy) /Parent 5 0 R /Dest (intro) >>",
            "<< /intro [4 0 R /Fit] >>",
        ]);
        assert_eq!(doc.outline()[0].dest.as_ref().unwrap().page, Some(1));
    }

    #[test]
    fn many_items_share_named_dest_resolution() {
        // Several bookmarks name the same (and a missing) destination; resolution
        // goes through the once-collected name map, not a per-item tree walk.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R /Names << /Dests 9 0 R >> >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 8 0 R /Count 3 >>",
            "<< /Title (A) /Parent 5 0 R /Next 7 0 R /Dest (sec) >>",
            "<< /Title (B) /Parent 5 0 R /Prev 6 0 R /Next 8 0 R /Dest (sec) >>",
            "<< /Title (C) /Parent 5 0 R /Prev 7 0 R /Dest (missing) >>",
            "<< /Names [ (sec) [4 0 R /Fit] ] >>",
        ]);
        let out = doc.outline();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].dest.as_ref().unwrap().page, Some(1));
        assert_eq!(out[1].dest.as_ref().unwrap().page, Some(1));
        assert!(out[2].dest.is_none(), "an unknown name resolves to no dest");
    }

    #[test]
    fn named_dest_in_outline_resolves() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R /Names << /Dests 7 0 R >> >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (By name) /Parent 5 0 R /Dest (sec1) >>",
            "<< /Names [ (sec1) [4 0 R /Fit] ] >>",
        ]);
        let dest = doc.outline()[0].dest.clone().expect("dest");
        assert_eq!(dest.page, Some(1));
    }

    #[test]
    fn sibling_cycle_terminates() {
        // /Next points back to the first item; the visited guard must stop it.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 7 0 R /Count 2 >>",
            "<< /Title (A) /Parent 5 0 R /Next 7 0 R >>",
            "<< /Title (B) /Parent 5 0 R /Next 6 0 R >>", // cycle back to A
        ]);
        let titles: Vec<_> = doc.outline().into_iter().map(|i| i.title).collect();
        assert_eq!(titles, ["A", "B"]); // each visited once, no hang
    }

    #[test]
    fn first_pointing_to_self_terminates() {
        // An item whose /First is itself: child recursion must not loop.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Type /Outlines /First 6 0 R /Last 6 0 R /Count 1 >>",
            "<< /Title (Self) /Parent 5 0 R /First 6 0 R >>",
        ]);
        let outline = doc.outline();
        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].title, "Self");
        assert!(
            outline[0].children.is_empty(),
            "self-child cut by visited set"
        );
    }
}
