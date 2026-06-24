//! Destinations (ISO 32000-1 §12.3.2): a location and view within the document
//! that a bookmark or link navigates to. A destination is either *explicit* —
//! an array `[page /Fit …]` naming a page object (or, for remote go-to actions,
//! a page *number*) and a view — or *named*: a name/string that indirects
//! through one of two registries to such an array.
//!
//! Named destinations live in two places, both consulted here:
//!
//! * the modern **`/Root /Names /Dests` name tree** (PDF 1.2+), whose values are
//!   either a destination array or a `<< /D array >>` dictionary, and
//! * the legacy **`/Root /Dests` dictionary** (a flat name → destination map),
//!   still emitted by older producers and by Word.
//!
//! This module resolves either form into a [`Destination`] carrying the target
//! page index (when the page reference belongs to this document's page tree),
//! the raw page reference, and the [`DestView`]. It only reads the object graph;
//! nothing here renders.

use std::collections::{HashMap, HashSet};

use zpdf_core::{ObjectId, PdfDict, PdfObject};
use zpdf_parser::PdfFile;

use crate::forms::pdf_string_to_unicode;
use crate::obj_util::{
    catalog_dict, resolve_array, resolve_dict, resolve_name, resolve_number, text,
};
use crate::Catalog;

/// Maximum depth of a `/Names /Dests` name-tree descent.
const MAX_NAME_TREE_DEPTH: usize = 64;
/// Global cap on name-tree nodes visited during one lookup — bounds an
/// adversarial (huge or cyclic-but-distinct) tree even within the depth limit.
const MAX_NAME_TREE_NODES: usize = 100_000;
/// Maximum chained name → name (or name → `/D` dict) indirections before giving
/// up — a destination should resolve in one or two hops; this bounds a cycle.
const MAX_DEST_INDIRECTION: usize = 8;
/// Cap on entries materialized when flattening the named-destination registries
/// once for a whole `outline()` walk (see [`collect_named_dests`]). Counts each
/// tree node and each collected entry, bounding the one-time collection of a
/// crafted `/Names` tree; far above any real document, which carries at most a
/// few thousand named destinations.
const MAX_NAMED_DEST_ENTRIES: usize = 200_000;

/// How a destination positions and zooms the target page (ISO 32000-1 Table
/// 151). Coordinates are in the page's default user space; `None` for a
/// coordinate means "retain the current value" (a `null` in the array).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DestView {
    /// `/XYZ left top zoom` — top-left at `(left, top)`, at `zoom` (a zoom of 0
    /// or `null` → `None`, meaning "retain current zoom").
    Xyz {
        left: Option<f32>,
        top: Option<f32>,
        zoom: Option<f32>,
    },
    /// `/Fit` — fit the whole page in the window.
    Fit,
    /// `/FitH top` — fit the page width, with `top` at the top of the window.
    FitH { top: Option<f32> },
    /// `/FitV left` — fit the page height, with `left` at the left edge.
    FitV { left: Option<f32> },
    /// `/FitR left bottom right top` — fit the given rectangle in the window.
    FitR {
        left: f32,
        bottom: f32,
        right: f32,
        top: f32,
    },
    /// `/FitB` — fit the page's bounding box (the bbox of its content).
    FitB,
    /// `/FitBH top` — fit the bounding-box width.
    FitBH { top: Option<f32> },
    /// `/FitBV left` — fit the bounding-box height.
    FitBV { left: Option<f32> },
    /// An unrecognized or malformed view mode (the page is still resolved).
    Unknown,
}

/// A resolved destination: where in the document to go, and how to view it.
#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    /// 0-based index of the target page, when its reference is a page in this
    /// document's page tree. `None` when the destination names a page object not
    /// in the tree, or gives a bare page *number* out of range (remote go-to).
    pub page: Option<usize>,
    /// The target page object reference, when the destination gave one (an
    /// explicit array whose first element is an indirect reference). `None` when
    /// the destination instead gave a page *number* (e.g. a remote go-to dest).
    pub page_ref: Option<ObjectId>,
    /// The view (zoom / fit) at the destination.
    pub view: DestView,
}

/// Resolve a named destination by its name (the bytes of a name object, or the
/// bytes of a name-tree string key). Tries the `/Names /Dests` name tree first,
/// then the legacy `/Root /Dests` dictionary. `None` if the name is unknown.
pub fn resolve_named(file: &PdfFile, catalog: &Catalog, name: &[u8]) -> Option<Destination> {
    let value = lookup_named_value(file, name)?;
    resolve_dest_value(file, catalog, &value, 0, None)
}

/// Resolve an explicit destination *value* — an array, a name/string (a named
/// destination), a `<< /D … >>` dictionary, or an indirect reference to any of
/// these. This is what a `/Dest` entry or an action's `/D` carries.
pub fn resolve_explicit(file: &PdfFile, catalog: &Catalog, obj: &PdfObject) -> Option<Destination> {
    resolve_dest_value(file, catalog, obj, 0, None)
}

/// Resolve a navigation target from a dictionary that may carry a `/Dest` and/or
/// an action `/A` — shared by the outline reader (bookmarks) and link-annotation
/// extraction. Returns `(destination, uri)`:
///
/// * a direct `/Dest`, or a go-to action (`/A /S /GoTo /D …`), yields the
///   [`Destination`];
/// * a URI action (`/A /S /URI /URI …`) yields the hyperlink string;
/// * a remote go-to (`/A /S /GoToR /F …`) yields the target *file name*.
///
/// `named`, when supplied, is the pre-collected named-destination map (see
/// [`collect_named_dests`]); resolving many targets against it is O(targets)
/// rather than O(targets × tree). Without the shared map a file with tens of
/// thousands of bookmarks/links each naming a (missing) destination over a
/// budget-sized, `/Limits`-free name tree would multiply the per-lookup node
/// budget by the target count into a multi-billion-node walk — a denial of
/// service. Passing `None` falls back to a single-shot bounded name-tree walk
/// per call (fine for one-off lookups).
pub(crate) fn resolve_link_target(
    file: &PdfFile,
    catalog: &Catalog,
    dict: &PdfDict,
    named: Option<&HashMap<Vec<u8>, PdfObject>>,
) -> (Option<Destination>, Option<String>) {
    // A direct /Dest takes precedence (a name, string, or explicit array).
    if let Some(dest_obj) = dict.get("Dest") {
        if let Some(d) = resolve_dest_value(file, catalog, dest_obj, 0, named) {
            return (Some(d), None);
        }
    }

    // Otherwise an action /A: go-to (a destination), URI (a hyperlink), or a
    // remote go-to (the destination file name).
    if let Some(action) = resolve_dict(file, dict.get("A")) {
        match resolve_name(file, action.get("S")).as_deref() {
            Some("GoTo") => {
                if let Some(d) = action
                    .get("D")
                    .and_then(|d| resolve_dest_value(file, catalog, d, 0, named))
                {
                    return (Some(d), None);
                }
            }
            Some("URI") => {
                if let Some(uri) = uri_string(file, &action) {
                    return (None, Some(uri));
                }
            }
            Some("GoToR") => {
                if let Some(name) = remote_file_name(file, &action) {
                    return (None, Some(name));
                }
            }
            _ => {}
        }
    }

    (None, None)
}

/// The `/URI` of a URI action — a byte string (often ASCII); decoded leniently.
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

/// Core resolver: turn any destination-shaped value into a [`Destination`],
/// following indirections (ref, named-string/name, `/D` dict) up to a bounded
/// depth so a self-referential name cannot loop. When `named` is supplied, a
/// name/string is resolved against that pre-collected map; otherwise it triggers
/// a fresh bounded name-tree walk.
fn resolve_dest_value(
    file: &PdfFile,
    catalog: &Catalog,
    obj: &PdfObject,
    depth: usize,
    named: Option<&HashMap<Vec<u8>, PdfObject>>,
) -> Option<Destination> {
    if depth > MAX_DEST_INDIRECTION {
        return None;
    }
    match obj {
        PdfObject::Array(arr) => parse_explicit_array(file, catalog, arr),
        PdfObject::Ref(r) => {
            let resolved = file.resolve(*r).ok()?;
            resolve_dest_value(file, catalog, &resolved, depth + 1, named)
        }
        // A named destination referenced by name (legacy) or string (name tree).
        PdfObject::Name(n) => {
            let v = lookup_in(file, named, n.as_str().as_bytes())?;
            resolve_dest_value(file, catalog, &v, depth + 1, named)
        }
        PdfObject::String(s) => {
            let v = lookup_in(file, named, s.as_bytes())?;
            resolve_dest_value(file, catalog, &v, depth + 1, named)
        }
        // A `<< /D [ … ] >>` destination dictionary (the name-tree value shape,
        // and the legacy-dict value shape).
        PdfObject::Dict(d) => {
            let inner = d.get("D")?;
            resolve_dest_value(file, catalog, inner, depth + 1, named)
        }
        _ => None,
    }
}

/// Look up a named destination's value: against the pre-collected map when the
/// outline walk supplies one (O(1), no re-walk), else via a fresh bounded
/// name-tree walk for a one-off [`resolve_named`] / [`resolve_explicit`] call.
fn lookup_in(
    file: &PdfFile,
    named: Option<&HashMap<Vec<u8>, PdfObject>>,
    name: &[u8],
) -> Option<PdfObject> {
    match named {
        Some(map) => map.get(name).cloned(),
        None => lookup_named_value(file, name),
    }
}

/// Parse an explicit destination array `[ pageRef /Fit … ]`. The first element
/// is the target page (an indirect reference for a local destination, or an
/// integer page number for a remote go-to); the rest name the view.
fn parse_explicit_array(
    file: &PdfFile,
    catalog: &Catalog,
    arr: &[PdfObject],
) -> Option<Destination> {
    if arr.is_empty() {
        return None;
    }
    let (page, page_ref) = match &arr[0] {
        PdfObject::Ref(r) => match catalog.page_index_of(*r) {
            Some(idx) => (Some(idx), Some(*r)),
            // Not a page in this tree. It may be an indirectly-encoded page
            // *number* (a rare remote-dest form); resolve once and try that
            // before reporting an unresolved page reference.
            None => match file.resolve(*r).ok().and_then(|o| page_number(&o, catalog)) {
                Some(idx) => (Some(idx), None),
                None => (None, Some(*r)),
            },
        },
        // A bare (remote/embedded go-to) page number, range-checked against the
        // document so an out-of-range index reports `None` per the contract.
        other => (page_number(other, catalog), None),
    };

    let view = parse_view(file, arr);
    Some(Destination {
        page,
        page_ref,
        view,
    })
}

/// A non-negative, in-range 0-based page index from a destination's first array
/// element when it is a bare page *number* (not a page reference). Out-of-range
/// or saturating values (a huge integer/real) yield `None`, so a destination's
/// `page` never points past the document.
fn page_number(obj: &PdfObject, catalog: &Catalog) -> Option<usize> {
    let n = match obj {
        PdfObject::Integer(n) if *n >= 0 => *n as usize,
        PdfObject::Real(f) if f.is_finite() && *f >= 0.0 && f.fract() == 0.0 => *f as usize,
        _ => return None,
    };
    (n < catalog.page_count).then_some(n)
}

/// Parse the view portion of a destination array (everything after the page).
fn parse_view(file: &PdfFile, arr: &[PdfObject]) -> DestView {
    // arr[1] is the fit-mode name; the numeric parameters follow.
    let num = |i: usize| resolve_number(file, arr.get(i));
    match resolve_name(file, arr.get(1)).as_deref() {
        Some("XYZ") => DestView::Xyz {
            left: num(2),
            top: num(3),
            // A zoom of 0 means "retain current zoom" — normalize to None.
            zoom: num(4).filter(|&z| z != 0.0),
        },
        Some("Fit") => DestView::Fit,
        Some("FitH") => DestView::FitH { top: num(2) },
        Some("FitV") => DestView::FitV { left: num(2) },
        Some("FitR") => DestView::FitR {
            left: num(2).unwrap_or(0.0),
            bottom: num(3).unwrap_or(0.0),
            right: num(4).unwrap_or(0.0),
            top: num(5).unwrap_or(0.0),
        },
        Some("FitB") => DestView::FitB,
        Some("FitBH") => DestView::FitBH { top: num(2) },
        Some("FitBV") => DestView::FitBV { left: num(2) },
        _ => DestView::Unknown,
    }
}

/// Look up a named destination's value (the array or `/D` dict it maps to),
/// trying the `/Names /Dests` name tree, then the legacy `/Root /Dests` dict.
fn lookup_named_value(file: &PdfFile, name: &[u8]) -> Option<PdfObject> {
    let root = catalog_dict(file)?;

    // Modern: /Root /Names /Dests name tree (string keys).
    if let Some(names) = resolve_dict(file, root.get("Names")) {
        if let Some(tree) = resolve_dict(file, names.get("Dests")) {
            let mut visited = HashSet::new();
            // Seed the cycle guard with the tree-root reference itself.
            if let Some(PdfObject::Ref(id)) = names.get("Dests") {
                visited.insert(*id);
            }
            let mut budget = MAX_NAME_TREE_NODES;
            if let Some(v) = name_tree_lookup(file, &tree, name, 0, &mut visited, &mut budget) {
                return Some(v);
            }
        }
    }

    // Legacy: /Root /Dests dictionary (name keys, direct name → destination).
    if let Some(dests) = resolve_dict(file, root.get("Dests")) {
        if let Ok(key) = std::str::from_utf8(name) {
            if let Some(v) = dests.get(key) {
                return Some(v.clone());
            }
        }
    }

    None
}

/// Search a name-tree node for `key`, returning its associated value. Descends
/// all children (robust to mis-sorted trees), bounded by depth, a per-reference
/// visited set, and a global node budget. `/Limits [lo hi]` prunes a subtree
/// only when present *and* well-formed — never at the cost of correctness.
fn name_tree_lookup(
    file: &PdfFile,
    node: &PdfDict,
    key: &[u8],
    depth: usize,
    visited: &mut HashSet<ObjectId>,
    budget: &mut usize,
) -> Option<PdfObject> {
    if depth > MAX_NAME_TREE_DEPTH || *budget == 0 {
        return None;
    }
    *budget -= 1;

    // Leaf: /Names [ key0 val0 key1 val1 … ], sorted by key.
    if let Some(names) = resolve_array(file, node.get("Names")) {
        let mut i = 0;
        while i + 1 < names.len() {
            if let PdfObject::String(s) = &names[i] {
                if s.as_bytes() == key {
                    return Some(names[i + 1].clone());
                }
            }
            i += 2;
        }
    }

    // Interior: /Kids [ refs ]. Prune by /Limits when it cleanly brackets the key.
    if let Some(kids) = resolve_array(file, node.get("Kids")) {
        for kid in &kids {
            if *budget == 0 {
                return None;
            }
            let kid_dict = match kid {
                PdfObject::Ref(r) => {
                    if !visited.insert(*r) {
                        continue;
                    }
                    resolve_dict(file, Some(kid))
                }
                PdfObject::Dict(_) => resolve_dict(file, Some(kid)),
                _ => None,
            };
            let Some(d) = kid_dict else { continue };
            if !limits_may_contain(file, &d, key) {
                continue;
            }
            if let Some(v) = name_tree_lookup(file, &d, key, depth + 1, visited, budget) {
                return Some(v);
            }
        }
    }

    None
}

/// Whether a node's `/Limits [lo hi]` could contain `key`. A missing or
/// malformed `/Limits` returns `true` (descend anyway), so the prune is only an
/// optimization and never hides an entry in a tree whose `/Limits` lie.
fn limits_may_contain(file: &PdfFile, node: &PdfDict, key: &[u8]) -> bool {
    let Some(limits) = resolve_array(file, node.get("Limits")) else {
        return true;
    };
    let (Some(PdfObject::String(lo)), Some(PdfObject::String(hi))) =
        (limits.first(), limits.get(1))
    else {
        return true;
    };
    // Name trees order keys by raw byte value.
    key >= lo.as_bytes() && key <= hi.as_bytes()
}

/// Flatten **both** named-destination registries — the `/Names /Dests` name tree
/// and the legacy `/Root /Dests` dictionary — into a single `name → value` map,
/// walked **once** and bounded by [`MAX_NAMED_DEST_ENTRIES`]. The outline walk
/// builds this once and resolves every bookmark's named destination against it
/// in O(1), instead of re-walking the tree per bookmark. Name-tree entries take
/// precedence over the legacy dict, and the first occurrence of a duplicate key
/// wins — matching [`lookup_named_value`]'s search order. Returns an empty map
/// (built instantly) when the document declares no named destinations.
pub(crate) fn collect_named_dests(file: &PdfFile) -> HashMap<Vec<u8>, PdfObject> {
    let mut map = HashMap::new();
    let Some(root) = catalog_dict(file) else {
        return map;
    };
    let mut budget = MAX_NAMED_DEST_ENTRIES;

    // Modern: /Root /Names /Dests name tree.
    if let Some(names) = resolve_dict(file, root.get("Names")) {
        if let Some(tree) = resolve_dict(file, names.get("Dests")) {
            let mut visited = HashSet::new();
            // Seed the cycle guard with the tree-root reference itself.
            if let Some(PdfObject::Ref(id)) = names.get("Dests") {
                visited.insert(*id);
            }
            collect_name_tree(file, &tree, 0, &mut visited, &mut budget, &mut map);
        }
    }

    // Legacy: /Root /Dests dictionary (name keys). Inserted only when absent, so
    // a name-tree entry of the same key wins (parity with lookup_named_value).
    if let Some(dests) = resolve_dict(file, root.get("Dests")) {
        for (k, v) in &dests.0 {
            if budget == 0 {
                break;
            }
            budget -= 1;
            map.entry(k.as_str().as_bytes().to_vec())
                .or_insert_with(|| v.clone());
        }
    }

    map
}

/// Recursively collect every leaf `name → value` entry from a name-tree node
/// into `map`, descending all `/Kids` (no `/Limits` pruning — we want every
/// entry). Bounded by depth, a per-reference visited set, and a shared budget
/// that counts each node **and each collected entry**, so one giant leaf or a
/// huge `/Kids` fan-out cannot run away. First occurrence of a key wins.
fn collect_name_tree(
    file: &PdfFile,
    node: &PdfDict,
    depth: usize,
    visited: &mut HashSet<ObjectId>,
    budget: &mut usize,
    map: &mut HashMap<Vec<u8>, PdfObject>,
) {
    if depth > MAX_NAME_TREE_DEPTH || *budget == 0 {
        return;
    }
    *budget -= 1;

    // Leaf: /Names [ key0 val0 key1 val1 … ].
    if let Some(names) = resolve_array(file, node.get("Names")) {
        let mut i = 0;
        while i + 1 < names.len() {
            if *budget == 0 {
                return;
            }
            if let PdfObject::String(s) = &names[i] {
                map.entry(s.as_bytes().to_vec())
                    .or_insert_with(|| names[i + 1].clone());
                *budget -= 1;
            }
            i += 2;
        }
    }

    // Interior: /Kids [ refs ].
    if let Some(kids) = resolve_array(file, node.get("Kids")) {
        for kid in &kids {
            if *budget == 0 {
                return;
            }
            let kid_dict = match kid {
                PdfObject::Ref(r) => {
                    if !visited.insert(*r) {
                        continue;
                    }
                    resolve_dict(file, Some(kid))
                }
                PdfObject::Dict(_) => resolve_dict(file, Some(kid)),
                _ => None,
            };
            let Some(d) = kid_dict else { continue };
            collect_name_tree(file, &d, depth + 1, visited, budget, map);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    fn open(objects: &[&str]) -> PdfDocument {
        PdfDocument::open(build_pdf(objects)).expect("open pdf")
    }

    // A two-page tree so page-reference resolution is observable (objects 2,3,4).
    const PAGES2: &str = "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>";
    const PAGE_A: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";
    const PAGE_B: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";

    #[test]
    fn named_dest_via_name_tree_xyz() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 5 0 R >> >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Names [ (chap2) << /D [4 0 R /XYZ 0 800 0] >> ] >>",
        ]);
        let d = doc.named_destination(b"chap2").expect("resolve");
        assert_eq!(d.page, Some(1)); // second page
        assert_eq!(d.page_ref, Some(zpdf_core::ObjectId(4, 0)));
        assert_eq!(
            d.view,
            DestView::Xyz {
                left: Some(0.0),
                top: Some(800.0),
                zoom: None, // a zoom of 0 normalizes to None
            }
        );
    }

    #[test]
    fn named_dest_via_legacy_root_dests_dict() {
        // Older producers register named dests directly under /Root /Dests.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Dests 5 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /intro [3 0 R /Fit] >>",
        ]);
        let d = doc.named_destination(b"intro").expect("resolve");
        assert_eq!(d.page, Some(0));
        assert_eq!(d.view, DestView::Fit);
    }

    #[test]
    fn named_dest_bare_array_value() {
        // Name-tree value is the bare array, not a /D dict.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 5 0 R >> >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Names [ (x) [4 0 R /FitH 750] ] >>",
        ]);
        let d = doc.named_destination(b"x").expect("resolve");
        assert_eq!(d.page, Some(1));
        assert_eq!(d.view, DestView::FitH { top: Some(750.0) });
    }

    #[test]
    fn name_tree_interior_kids_with_limits() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 5 0 R >> >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Kids [6 0 R 7 0 R] >>",
            "<< /Limits [(a) (m)] /Names [ (b) [3 0 R /Fit] ] >>",
            "<< /Limits [(n) (z)] /Names [ (y) [4 0 R /Fit] ] >>",
        ]);
        // Key in the second leaf's range, reachable past the first.
        let d = doc.named_destination(b"y").expect("resolve");
        assert_eq!(d.page, Some(1));
    }

    #[test]
    fn explicit_fitr_all_coords() {
        let doc = open(&["<< /Type /Catalog /Pages 2 0 R >>", PAGES2, PAGE_A, PAGE_B]);
        let arr = PdfObject::Array(vec![
            PdfObject::Ref(zpdf_core::ObjectId(3, 0)),
            PdfObject::Name(zpdf_core::PdfName("FitR".into())),
            PdfObject::Integer(10),
            PdfObject::Integer(20),
            PdfObject::Integer(30),
            PdfObject::Integer(40),
        ]);
        let d = doc.resolve_destination(&arr).expect("resolve");
        assert_eq!(d.page, Some(0));
        assert_eq!(
            d.view,
            DestView::FitR {
                left: 10.0,
                bottom: 20.0,
                right: 30.0,
                top: 40.0,
            }
        );
    }

    #[test]
    fn explicit_page_number_for_remote_dest() {
        // First element is an integer (remote go-to): it's already a page index,
        // with no page_ref into this document.
        let doc = open(&["<< /Type /Catalog /Pages 2 0 R >>", PAGES2, PAGE_A, PAGE_B]);
        let arr = PdfObject::Array(vec![
            PdfObject::Integer(1),
            PdfObject::Name(zpdf_core::PdfName("Fit".into())),
        ]);
        let d = doc.resolve_destination(&arr).expect("resolve");
        assert_eq!(d.page, Some(1));
        assert_eq!(d.page_ref, None);
    }

    #[test]
    fn out_of_range_page_number_is_none() {
        // A bare page number past the last page (a malformed or remote dest)
        // must report page None, per the documented contract — not a bogus index.
        let doc = open(&["<< /Type /Catalog /Pages 2 0 R >>", PAGES2, PAGE_A, PAGE_B]);
        // 2-page document: index 2 and beyond are out of range.
        let arr = PdfObject::Array(vec![
            PdfObject::Integer(500),
            PdfObject::Name(zpdf_core::PdfName("Fit".into())),
        ]);
        assert_eq!(doc.resolve_destination(&arr).unwrap().page, None);
        // A saturating real must not become usize::MAX.
        let big = PdfObject::Array(vec![
            PdfObject::Real(1e20),
            PdfObject::Name(zpdf_core::PdfName("Fit".into())),
        ]);
        assert_eq!(doc.resolve_destination(&big).unwrap().page, None);
    }

    #[test]
    fn indirectly_encoded_page_number_resolves() {
        // First element is an indirect ref to a bare integer page number (a rare
        // remote-dest form), not a page object: resolve it to the page index.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "1", // object 5: the page number
        ]);
        let arr = PdfObject::Array(vec![
            PdfObject::Ref(zpdf_core::ObjectId(5, 0)),
            PdfObject::Name(zpdf_core::PdfName("Fit".into())),
        ]);
        let d = doc.resolve_destination(&arr).expect("resolve");
        assert_eq!(d.page, Some(1));
        assert_eq!(d.page_ref, None);
    }

    #[test]
    fn page_ref_not_in_tree_yields_none_page_but_keeps_view() {
        let doc = open(&["<< /Type /Catalog /Pages 2 0 R >>", PAGES2, PAGE_A, PAGE_B]);
        let arr = PdfObject::Array(vec![
            PdfObject::Ref(zpdf_core::ObjectId(999, 0)), // not a page
            PdfObject::Name(zpdf_core::PdfName("Fit".into())),
        ]);
        let d = doc.resolve_destination(&arr).expect("resolve");
        assert_eq!(d.page, None);
        assert_eq!(d.page_ref, Some(zpdf_core::ObjectId(999, 0)));
        assert_eq!(d.view, DestView::Fit);
    }

    #[test]
    fn unknown_name_resolves_to_none() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 5 0 R >> >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Names [ (real) [3 0 R /Fit] ] >>",
        ]);
        assert!(doc.named_destination(b"missing").is_none());
    }

    #[test]
    fn self_referential_named_dest_terminates() {
        // A name whose value is its own name: the indirection guard must stop it.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 5 0 R >> >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
            "<< /Names [ (loop) (loop) ] >>",
        ]);
        assert!(doc.named_destination(b"loop").is_none());
    }
}
