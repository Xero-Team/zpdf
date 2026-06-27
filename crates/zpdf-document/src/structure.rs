//! Logical structure tree / Tagged PDF (ISO 32000-1 §14.7–14.8). A *tagged* PDF
//! carries, alongside its page content, a tree of **structure elements** that
//! describes the document's logical organization — its headings, paragraphs,
//! lists, tables, figures and their reading order — independent of how that
//! content happens to be laid out on the page. This is what makes a PDF
//! accessible (a screen reader walks the structure tree, not the page) and what
//! lets a consumer recover semantic structure rather than a bag of glyphs.
//!
//! The catalog's `/StructTreeRoot` roots the tree (§14.7.2). Each node is a
//! *structure element* dictionary (`/Type /StructElem`) carrying:
//!
//! * `/S` — the **structure type** (the *role*): a standard type such as `P`,
//!   `H1`, `Table`, `Figure`, or a producer-defined type mapped to a standard
//!   one by the root's `/RoleMap`.
//! * `/K` — the element's **kids**, a single value or an array mixing: nested
//!   structure elements (by reference or inline dict), bare integers
//!   (*marked-content identifiers* — MCIDs — pointing into a page's content
//!   stream), marked-content reference dicts (`/Type /MCR`), and object
//!   reference dicts (`/Type /OBJR`, e.g. for an annotation).
//! * `/Pg` — the page whose content stream the element's MCIDs index; inherited
//!   by descendants that don't carry their own.
//! * `/Alt`, `/ActualText`, `/E`, `/T`, `/Lang` — accessibility text and
//!   metadata: an alternate description, the exact replacement text, an
//!   abbreviation expansion, a title, and a language tag.
//!
//! This module reads `/StructTreeRoot` once into a navigable [`StructTree`]. Like
//! the other document readers it only walks the object graph — nothing here
//! renders — and it runs only when explicitly called, never during `open` or
//! rendering. Every descent is bounded (depth cap, a per-reference visited set
//! seeded with the tree-root reference, a shared node/entry budget, role-map
//! resolution depth, and per-string length caps) so a malformed or adversarial
//! tree cannot hang, recurse without bound, or exhaust memory.

use std::collections::{HashMap, HashSet};

use zpdf_core::{ObjectId, PdfDict, PdfObject};
use zpdf_parser::PdfFile;

use crate::obj_util::{catalog_dict, resolve_dict, text};
use crate::Catalog;

/// Maximum nesting depth of the structure tree before a subtree is pruned
/// (mirrors the outline/number-tree bounds used elsewhere).
const MAX_STRUCT_DEPTH: usize = 64;
/// Cap on structure elements *and* kid-array entries materialized while walking
/// the tree — a shared budget that bounds a crafted huge or densely-referenced
/// tree. Far above any real document.
const MAX_STRUCT_ELEMENTS: usize = 500_000;
/// Cap on transitive `/RoleMap` resolution (custom → … → standard type), so a
/// `RoleMap` that maps a name through a cycle cannot loop.
const MAX_ROLE_MAP_DEPTH: usize = 32;
/// Cap on `/RoleMap` entries read — a real map has a handful; this bounds a
/// crafted one.
const MAX_ROLE_MAP_ENTRIES: usize = 65_536;
/// Cap (in `char`s) on a per-element text string (`/Alt`, `/ActualText`, `/T`,
/// `/E`, `/Lang`). Real values are short; this bounds an adversarial one.
const MAX_TEXT_CHARS: usize = 64 * 1024;

/// A standard structure type (ISO 32000-1 §14.8.4), the *role* of a structure
/// element after resolving its `/S` through the document's `/RoleMap`. A type
/// outside the standard set (and not mapped onto it) is [`StructRole::Other`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructRole {
    // Grouping elements (§14.8.4.3).
    Document,
    Part,
    Art,
    Sect,
    Div,
    BlockQuote,
    Caption,
    /// Table of contents (`TOC`).
    Toc,
    /// Table-of-contents item (`TOCI`).
    Toci,
    Index,
    NonStruct,
    Private,
    // Paragraphlike block-level elements (§14.8.4.4).
    P,
    H,
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
    // List elements.
    L,
    /// List item (`LI`).
    Li,
    /// List-item label (`Lbl`).
    Lbl,
    /// List-item body (`LBody`).
    LBody,
    // Table elements.
    Table,
    /// Table row (`TR`).
    Tr,
    /// Table header cell (`TH`).
    Th,
    /// Table data cell (`TD`).
    Td,
    /// Table header row group (`THead`).
    THead,
    /// Table body row group (`TBody`).
    TBody,
    /// Table footer row group (`TFoot`).
    TFoot,
    // Inline-level elements (§14.8.4.5).
    Span,
    Quote,
    Note,
    Reference,
    BibEntry,
    Code,
    Link,
    Annot,
    // Ruby / Warichu sub-elements.
    Ruby,
    /// Ruby base text (`RB`).
    Rb,
    /// Ruby annotation text (`RT`).
    Rt,
    /// Ruby punctuation (`RP`).
    Rp,
    Warichu,
    /// Warichu text (`WT`).
    Wt,
    /// Warichu punctuation (`WP`).
    Wp,
    // Illustration elements (§14.8.4.6).
    Figure,
    Formula,
    Form,
    /// A non-standard type: the resolved type name (possibly the producer's own,
    /// when `/RoleMap` did not map it onto a standard type).
    Other(String),
}

impl StructRole {
    /// Classify a (role-map-resolved) structure-type *name* into a standard role,
    /// or [`StructRole::Other`] when it is not one of the standard types.
    fn from_name(name: &str) -> Self {
        use StructRole::*;
        match name {
            "Document" => Document,
            "Part" => Part,
            "Art" => Art,
            "Sect" => Sect,
            "Div" => Div,
            "BlockQuote" => BlockQuote,
            "Caption" => Caption,
            "TOC" => Toc,
            "TOCI" => Toci,
            "Index" => Index,
            "NonStruct" => NonStruct,
            "Private" => Private,
            "P" => P,
            "H" => H,
            "H1" => H1,
            "H2" => H2,
            "H3" => H3,
            "H4" => H4,
            "H5" => H5,
            "H6" => H6,
            "L" => L,
            "LI" => Li,
            "Lbl" => Lbl,
            "LBody" => LBody,
            "Table" => Table,
            "TR" => Tr,
            "TH" => Th,
            "TD" => Td,
            "THead" => THead,
            "TBody" => TBody,
            "TFoot" => TFoot,
            "Span" => Span,
            "Quote" => Quote,
            "Note" => Note,
            "Reference" => Reference,
            "BibEntry" => BibEntry,
            "Code" => Code,
            "Link" => Link,
            "Annot" => Annot,
            "Ruby" => Ruby,
            "RB" => Rb,
            "RT" => Rt,
            "RP" => Rp,
            "Warichu" => Warichu,
            "WT" => Wt,
            "WP" => Wp,
            "Figure" => Figure,
            "Formula" => Formula,
            "Form" => Form,
            other => Other(other.to_string()),
        }
    }

    /// The canonical PDF type name for this role (`Toc` → `"TOC"`, `Li` → `"LI"`,
    /// …). For [`StructRole::Other`] this is the resolved producer type name.
    pub fn as_str(&self) -> &str {
        use StructRole::*;
        match self {
            Document => "Document",
            Part => "Part",
            Art => "Art",
            Sect => "Sect",
            Div => "Div",
            BlockQuote => "BlockQuote",
            Caption => "Caption",
            Toc => "TOC",
            Toci => "TOCI",
            Index => "Index",
            NonStruct => "NonStruct",
            Private => "Private",
            P => "P",
            H => "H",
            H1 => "H1",
            H2 => "H2",
            H3 => "H3",
            H4 => "H4",
            H5 => "H5",
            H6 => "H6",
            L => "L",
            Li => "LI",
            Lbl => "Lbl",
            LBody => "LBody",
            Table => "Table",
            Tr => "TR",
            Th => "TH",
            Td => "TD",
            THead => "THead",
            TBody => "TBody",
            TFoot => "TFoot",
            Span => "Span",
            Quote => "Quote",
            Note => "Note",
            Reference => "Reference",
            BibEntry => "BibEntry",
            Code => "Code",
            Link => "Link",
            Annot => "Annot",
            Ruby => "Ruby",
            Rb => "RB",
            Rt => "RT",
            Rp => "RP",
            Warichu => "Warichu",
            Wt => "WT",
            Wp => "WP",
            Figure => "Figure",
            Formula => "Formula",
            Form => "Form",
            Other(s) => s,
        }
    }

    /// True when this is a recognized standard structure type (not
    /// [`StructRole::Other`]).
    pub fn is_standard(&self) -> bool {
        !matches!(self, StructRole::Other(_))
    }

    /// True for a heading role (`H` or `H1`…`H6`).
    pub fn is_heading(&self) -> bool {
        use StructRole::*;
        matches!(self, H | H1 | H2 | H3 | H4 | H5 | H6)
    }

    /// True for a block-level role — a grouping element or a block-level
    /// structure element (paragraph, heading, list item, table row, figure …)
    /// that begins on its own line when serializing the tree to reading-ordered
    /// text. Inline-level roles (`Span`, `Quote`, `Link`, `Reference`,
    /// ruby/warichu …), the transparent `NonStruct`/`Private` grouping, and the
    /// *intra-line* structure roles flow inline: a list item's label and body
    /// (`Lbl`/`LBody`) read on one line with the item, and a row's cells
    /// (`TH`/`TD`, and the `THead`/`TBody`/`TFoot` row groups) read across the
    /// row rather than each on its own line.
    pub fn is_block_level(&self) -> bool {
        use StructRole::*;
        matches!(
            self,
            Document
                | Part
                | Art
                | Sect
                | Div
                | BlockQuote
                | Caption
                | Toc
                | Toci
                | Index
                | P
                | H
                | H1
                | H2
                | H3
                | H4
                | H5
                | H6
                | L
                | Li
                | Table
                | Tr
                // `Note`: spec-inline (§14.8.4.5), but a footnote/endnote reads
                // as its own block in logical order, like mainstream extractors.
                | Note
                | Figure
                | Formula
        )
    }
}

/// A kid of a structure element: a nested element, a marked-content sequence in
/// a page's content stream, or a whole referenced object (`/OBJR`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructKid {
    /// A nested structure element.
    Element(StructElem),
    /// A marked-content sequence — a bare integer kid or a `/Type /MCR` dict.
    /// `page` is the 0-based index of the page whose content stream `mcid`
    /// indexes (the element's effective `/Pg`), or `None` when unresolved.
    MarkedContent {
        /// 0-based page index of the content stream this MCID indexes.
        page: Option<usize>,
        /// The marked-content identifier (`/MCID`).
        mcid: i64,
    },
    /// A reference to a whole object (`/Type /OBJR`), e.g. an annotation that
    /// participates in the logical structure.
    Object {
        /// 0-based page index the object appears on (`/Pg`), when resolvable.
        page: Option<usize>,
        /// The referenced object.
        obj: ObjectId,
    },
}

/// One structure element (`/Type /StructElem`): a node of the logical tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructElem {
    /// `/S` resolved through `/RoleMap` and classified into a standard role.
    pub role: StructRole,
    /// The original `/S` type name, before `/RoleMap` resolution (equals
    /// [`StructRole::as_str`] for a standard, unmapped type).
    pub raw_type: String,
    /// `/T` — an optional human-readable title for the element.
    pub title: Option<String>,
    /// `/Lang` — a BCP 47 language tag scoping this subtree (e.g. `"en-US"`).
    pub lang: Option<String>,
    /// `/Alt` — an alternate textual description (accessibility), e.g. for a
    /// [`StructRole::Figure`].
    pub alt: Option<String>,
    /// `/ActualText` — the exact text this element stands in for (e.g. a ligature
    /// or an image of text).
    pub actual_text: Option<String>,
    /// `/E` — the expansion of an abbreviation or acronym carried by this element.
    pub expansion: Option<String>,
    /// The element's effective page: its own `/Pg`, or the nearest ancestor's,
    /// as a 0-based index. `None` when none is declared/resolvable.
    pub page: Option<usize>,
    /// The element's children (`/K`).
    pub kids: Vec<StructKid>,
}

impl StructElem {
    /// The accessibility text this element directly provides: `/ActualText` (the
    /// exact replacement) if present, else `/Alt` (an alternate description).
    pub fn accessible_text(&self) -> Option<&str> {
        self.actual_text.as_deref().or(self.alt.as_deref())
    }

    /// Nested structure-element children (skipping marked-content / object kids).
    pub fn child_elements(&self) -> impl Iterator<Item = &StructElem> {
        self.kids.iter().filter_map(|k| match k {
            StructKid::Element(e) => Some(e),
            _ => None,
        })
    }
}

/// The document's logical structure tree (`/StructTreeRoot`). Present only when
/// the catalog declares one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructTree {
    /// The top-level structure elements (the root's `/K`).
    pub children: Vec<StructElem>,
    /// `/MarkInfo /Marked true` — the document declares Tagged-PDF conformance.
    pub marked: bool,
}

impl StructTree {
    /// Total number of structure elements in the tree (all nesting levels).
    pub fn element_count(&self) -> usize {
        fn count(e: &StructElem) -> usize {
            1 + e.child_elements().map(count).sum::<usize>()
        }
        self.children.iter().map(count).sum()
    }
}

/// Whether the document declares Tagged-PDF conformance via the catalog's
/// `/MarkInfo` dictionary (`/Marked true`). Independent of whether a
/// `/StructTreeRoot` is actually present.
pub fn is_tagged(file: &PdfFile) -> bool {
    let Some(root) = catalog_dict(file) else {
        return false;
    };
    let Some(mark_info) = resolve_dict(file, root.get("MarkInfo")) else {
        return false;
    };
    matches!(mark_info.get("Marked"), Some(PdfObject::Bool(true)))
}

/// Parse the document's logical structure tree from the catalog's
/// `/StructTreeRoot`. Returns `None` when the document declares no structure
/// tree (i.e. is not tagged in the structural sense).
pub fn parse_struct_tree(file: &PdfFile, catalog: &Catalog) -> Option<StructTree> {
    let root = catalog_dict(file)?;
    let tree_root = resolve_dict(file, root.get("StructTreeRoot"))?;

    let mut visited = HashSet::new();
    // Seed the cycle guard with the tree-root reference, so a kid pointing back
    // at the root cannot spawn a spurious element (parity with outline/page-label
    // tree-root seeding).
    if let Some(PdfObject::Ref(id)) = root.get("StructTreeRoot") {
        visited.insert(*id);
    }

    let mut walk = StructWalk {
        file,
        catalog,
        role_map: read_role_map(file, &tree_root),
        visited,
        budget: MAX_STRUCT_ELEMENTS,
    };

    // The root's /K holds the top-level structure element(s). Its own /Pg (rare)
    // seeds page inheritance.
    let root_page = walk.page_of(&tree_root);
    let mut children = Vec::new();
    for kid in normalize_kids(file, &tree_root) {
        if let Some(StructKid::Element(e)) = walk.parse_kid(&kid, root_page, 0) {
            children.push(e);
        }
        // A bare MCID / OBJR directly under the root is non-conformant; drop it.
    }

    Some(StructTree {
        children,
        marked: is_tagged(file),
    })
}

/// Shared state for one structure-tree traversal.
struct StructWalk<'a> {
    file: &'a PdfFile,
    catalog: &'a Catalog,
    /// `/RoleMap`: producer type name → mapped-to type name.
    role_map: HashMap<String, String>,
    /// Structure-element references seen so far — a `/K` back-edge to any of them
    /// terminates that branch (cycle / shared-subtree guard).
    visited: HashSet<ObjectId>,
    /// Remaining element + kid-entry budget (shared across the whole walk).
    budget: usize,
}

impl StructWalk<'_> {
    /// Parse one `/K` entry into a [`StructKid`]. Spends one unit of the shared
    /// budget per entry examined (so a giant `/K` array can't be scanned for
    /// free), and bounds element recursion by depth and the visited set.
    fn parse_kid(
        &mut self,
        obj: &PdfObject,
        parent_page: Option<usize>,
        depth: usize,
    ) -> Option<StructKid> {
        if self.budget == 0 {
            return None;
        }
        self.budget -= 1;

        match obj {
            // A bare integer kid is an MCID into the parent's effective page.
            PdfObject::Integer(mcid) => Some(StructKid::MarkedContent {
                page: parent_page,
                mcid: *mcid,
            }),

            // A reference: to a nested structure element, or (rarely) to an
            // MCR/OBJR dict.
            PdfObject::Ref(id) => {
                let resolved = self.file.resolve(*id).ok()?;
                let dict = resolved.as_dict().ok()?;
                match kid_dict_kind(dict) {
                    KidKind::Mcr => self.marked_content(dict, parent_page),
                    KidKind::Objr => self.object_ref(dict, parent_page),
                    KidKind::Element => {
                        // Cycle / shared-subtree guard on element identity.
                        if !self.visited.insert(*id) {
                            return None;
                        }
                        self.element(dict, parent_page, depth)
                            .map(StructKid::Element)
                    }
                }
            }

            // An inline dict kid: an MCR, an OBJR, or a nested element.
            PdfObject::Dict(dict) => match kid_dict_kind(dict) {
                KidKind::Mcr => self.marked_content(dict, parent_page),
                KidKind::Objr => self.object_ref(dict, parent_page),
                KidKind::Element => self
                    .element(dict, parent_page, depth)
                    .map(StructKid::Element),
            },

            _ => None,
        }
    }

    /// Build a [`StructElem`] from its dictionary, recursing into `/K`.
    fn element(
        &mut self,
        dict: &PdfDict,
        parent_page: Option<usize>,
        depth: usize,
    ) -> Option<StructElem> {
        if depth > MAX_STRUCT_DEPTH {
            return None;
        }
        // Effective page: this element's /Pg, else inherited from the ancestor.
        let page = self.page_of(dict).or(parent_page);

        let raw_type = self.file_name(dict, "S").unwrap_or_default();
        let role = StructRole::from_name(&self.resolve_role(&raw_type));

        let kids = normalize_kids(self.file, dict)
            .iter()
            .filter_map(|k| self.parse_kid(k, page, depth + 1))
            .collect();

        Some(StructElem {
            role,
            raw_type,
            title: capped_text(self.file, dict, "T"),
            lang: capped_text(self.file, dict, "Lang"),
            alt: capped_text(self.file, dict, "Alt"),
            actual_text: capped_text(self.file, dict, "ActualText"),
            expansion: capped_text(self.file, dict, "E"),
            page,
            kids,
        })
    }

    /// Build a [`StructKid::MarkedContent`] from a `/MCR` dict (or a kid dict
    /// treated as one). An MCR may carry its own `/Pg`, overriding the inherited
    /// page; its `/MCID` is required.
    fn marked_content(&self, dict: &PdfDict, parent_page: Option<usize>) -> Option<StructKid> {
        let mcid = int_value(dict.get("MCID"))?;
        let page = self.page_of(dict).or(parent_page);
        Some(StructKid::MarkedContent { page, mcid })
    }

    /// Build a [`StructKid::Object`] from an `/OBJR` dict. `/Obj` (the referenced
    /// object) is required; `/Pg` overrides the inherited page.
    fn object_ref(&self, dict: &PdfDict, parent_page: Option<usize>) -> Option<StructKid> {
        let obj = dict.get_ref("Obj").ok()?;
        let page = self.page_of(dict).or(parent_page);
        Some(StructKid::Object { page, obj })
    }

    /// Resolve a dict's `/Pg` (a page reference) to a 0-based page index.
    fn page_of(&self, dict: &PdfDict) -> Option<usize> {
        let pg = dict.get_ref("Pg").ok()?;
        self.catalog.page_index_of(pg)
    }

    /// Read a Name-valued entry, following one indirect reference.
    fn file_name(&self, dict: &PdfDict, key: &str) -> Option<String> {
        crate::obj_util::name_value(self.file, dict, key)
    }

    /// Resolve a structure type through `/RoleMap`, transitively, until it maps
    /// to a name not further mapped (or a cycle / the depth cap stops it).
    fn resolve_role(&self, raw: &str) -> String {
        let mut current = raw.to_string();
        let mut seen = HashSet::new();
        for _ in 0..MAX_ROLE_MAP_DEPTH {
            if !seen.insert(current.clone()) {
                break;
            }
            match self.role_map.get(&current) {
                Some(next) if next != &current => current = next.clone(),
                _ => break,
            }
        }
        current
    }
}

/// How a `/K` kid dictionary should be interpreted.
enum KidKind {
    /// `/Type /MCR` (or a kid dict carrying an `/MCID` and no `/S`).
    Mcr,
    /// `/Type /OBJR` (or a kid dict carrying an `/Obj` and no `/S`).
    Objr,
    /// A nested structure element.
    Element,
}

/// Classify a `/K` kid dictionary. `/Type` is authoritative; absent it, an
/// `/MCID`-only dict is treated as an MCR and an `/Obj`-only dict as an OBJR
/// (lax producers), and everything else as a structure element.
fn kid_dict_kind(dict: &PdfDict) -> KidKind {
    match dict.get_name("Type") {
        Ok("MCR") => return KidKind::Mcr,
        Ok("OBJR") => return KidKind::Objr,
        Ok("StructElem") => return KidKind::Element,
        _ => {}
    }
    // No (recognized) /Type: infer from the keys present. A structure element is
    // identified by /S; without it, /MCID → MCR and /Obj → OBJR.
    if dict.get("S").is_none() {
        if dict.get("MCID").is_some() {
            return KidKind::Mcr;
        }
        if dict.get("Obj").is_some() {
            return KidKind::Objr;
        }
    }
    KidKind::Element
}

/// Normalize a dictionary's `/K` into a flat list of kid objects: a single value
/// becomes a one-element list, an array is taken as-is, and an indirect `/K`
/// array is resolved. A `/K` that is an indirect reference to a *single*
/// structure element is kept as that reference (so the visited-set guard sees its
/// object identity).
fn normalize_kids(file: &PdfFile, dict: &PdfDict) -> Vec<PdfObject> {
    match dict.get("K") {
        Some(PdfObject::Array(a)) => a.clone(),
        Some(PdfObject::Ref(r)) => match file.resolve(*r) {
            Ok(PdfObject::Array(a)) => a,
            // A ref to a non-array (a single element/MCR/OBJR): keep the ref.
            Ok(_) => vec![PdfObject::Ref(*r)],
            Err(_) => Vec::new(),
        },
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    }
}

/// Read `/RoleMap` (producer type name → standard type name) into a map, bounded
/// in size. Only Name → Name entries are kept.
fn read_role_map(file: &PdfFile, tree_root: &PdfDict) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(rm) = resolve_dict(file, tree_root.get("RoleMap")) else {
        return map;
    };
    for (key, value) in rm.0.iter() {
        if map.len() >= MAX_ROLE_MAP_ENTRIES {
            break;
        }
        if let PdfObject::Name(n) = value {
            map.insert(key.as_str().to_string(), n.as_str().to_string());
        }
    }
    map
}

/// An integer object (a bare `/K` MCID, or `/MCID`), accepting a whole-valued
/// real for lax producers.
fn int_value(obj: Option<&PdfObject>) -> Option<i64> {
    match obj? {
        PdfObject::Integer(n) => Some(*n),
        PdfObject::Real(f) if f.is_finite() && f.fract() == 0.0 => Some(*f as i64),
        _ => None,
    }
}

/// Read a text-string entry and cap it to [`MAX_TEXT_CHARS`] on a `char`
/// boundary (an adversarial `/Alt`/`/ActualText` can be arbitrarily long).
fn capped_text(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<String> {
    match text(file, dict, key) {
        Some(s) if s.chars().count() > MAX_TEXT_CHARS => {
            Some(s.chars().take(MAX_TEXT_CHARS).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    const PAGES: &str = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    const PAGE: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";

    fn open(objects: &[&str]) -> PdfDocument {
        PdfDocument::open(build_pdf(objects)).expect("open pdf")
    }

    /// Build a document whose catalog (object 1) is `catalog`, with the standard
    /// one-page tree, followed by `extra` structure objects (objects 4, 5, …).
    fn doc(catalog: &str, extra: &[&str]) -> PdfDocument {
        let mut objs = vec![catalog, PAGES, PAGE];
        objs.extend_from_slice(extra);
        open(&objs)
    }

    #[test]
    fn no_struct_tree_is_none() {
        let d = doc("<< /Type /Catalog /Pages 2 0 R >>", &[]);
        assert!(d.struct_tree().is_none());
        assert!(!d.is_tagged());
    }

    #[test]
    fn mark_info_marks_tagged() {
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /MarkInfo << /Marked true >> >>",
            &[],
        );
        assert!(d.is_tagged());
        // MarkInfo without a StructTreeRoot still has no tree.
        assert!(d.struct_tree().is_none());
    }

    #[test]
    fn simple_document_paragraph_with_mcids() {
        // StructTreeRoot(4) -> Document(5) -> P(6) with two MCID kids, /Pg = page 0.
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R \
             /MarkInfo << /Marked true >> >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /Document /P 4 0 R /K 6 0 R >>",
                "<< /Type /StructElem /S /P /P 5 0 R /Pg 3 0 R /K [0 1] >>",
            ],
        );
        let tree = d.struct_tree().expect("tree");
        assert!(tree.marked);
        assert_eq!(tree.children.len(), 1);
        let document = &tree.children[0];
        assert_eq!(document.role, StructRole::Document);
        assert_eq!(document.kids.len(), 1);

        let para = document.child_elements().next().unwrap();
        assert_eq!(para.role, StructRole::P);
        assert_eq!(para.page, Some(0));
        assert_eq!(
            para.kids,
            vec![
                StructKid::MarkedContent {
                    page: Some(0),
                    mcid: 0
                },
                StructKid::MarkedContent {
                    page: Some(0),
                    mcid: 1
                },
            ]
        );
        assert_eq!(tree.element_count(), 2);
    }

    #[test]
    fn role_map_resolves_custom_type() {
        // A producer type "Heading1" mapped onto the standard /H1 by /RoleMap.
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R /RoleMap << /Heading1 /H1 >> >>",
                "<< /Type /StructElem /S /Heading1 /P 4 0 R >>",
            ],
        );
        let tree = d.struct_tree().expect("tree");
        let h = &tree.children[0];
        assert_eq!(h.role, StructRole::H1);
        assert!(h.role.is_heading());
        assert_eq!(h.raw_type, "Heading1"); // original /S preserved
    }

    #[test]
    fn unmapped_custom_type_is_other() {
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /MyWidget /P 4 0 R >>",
            ],
        );
        let role = &d.struct_tree().unwrap().children[0].role;
        assert_eq!(role, &StructRole::Other("MyWidget".to_string()));
        assert!(!role.is_standard());
        assert_eq!(role.as_str(), "MyWidget");
    }

    #[test]
    fn figure_alt_text_is_accessible() {
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /Figure /P 4 0 R /Alt (A bar chart) >>",
            ],
        );
        let fig = &d.struct_tree().unwrap().children[0];
        assert_eq!(fig.role, StructRole::Figure);
        assert_eq!(fig.alt.as_deref(), Some("A bar chart"));
        assert_eq!(fig.accessible_text(), Some("A bar chart"));
    }

    #[test]
    fn actual_text_preferred_over_alt() {
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /Span /P 4 0 R /Alt (alt) /ActualText (exact) >>",
            ],
        );
        let span = &d.struct_tree().unwrap().children[0];
        assert_eq!(span.accessible_text(), Some("exact"));
    }

    #[test]
    fn objr_kid_resolves_object_and_page() {
        // A kid /OBJR referencing an annotation object on page 0.
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /Link /P 4 0 R \
                 /K << /Type /OBJR /Obj 6 0 R /Pg 3 0 R >> >>",
                "<< /Type /Annot /Subtype /Link >>",
            ],
        );
        let link = &d.struct_tree().unwrap().children[0];
        assert_eq!(link.role, StructRole::Link);
        assert_eq!(link.kids.len(), 1);
        match &link.kids[0] {
            StructKid::Object { page, obj } => {
                assert_eq!(*page, Some(0));
                assert_eq!(obj.0, 6); // object number 6
            }
            other => panic!("expected OBJR kid, got {other:?}"),
        }
    }

    #[test]
    fn mcr_dict_kid_with_explicit_page() {
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /P /P 4 0 R \
                 /K << /Type /MCR /Pg 3 0 R /MCID 7 >> >>",
            ],
        );
        let para = &d.struct_tree().unwrap().children[0];
        assert_eq!(
            para.kids[0],
            StructKid::MarkedContent {
                page: Some(0),
                mcid: 7
            }
        );
    }

    #[test]
    fn page_inherited_from_ancestor() {
        // The inner Span has no /Pg; it inherits the Sect's /Pg for its MCID kid.
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /Sect /P 4 0 R /Pg 3 0 R /K 6 0 R >>",
                "<< /Type /StructElem /S /Span /P 5 0 R /K [9] >>",
            ],
        );
        let span = d.struct_tree().unwrap().children[0]
            .child_elements()
            .next()
            .unwrap()
            .clone();
        assert_eq!(span.page, Some(0), "inherited /Pg");
        assert_eq!(
            span.kids[0],
            StructKid::MarkedContent {
                page: Some(0),
                mcid: 9
            }
        );
    }

    #[test]
    fn single_ref_k_not_array() {
        // /K as a single reference (not an array) is honoured.
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /Document /K 6 0 R >>",
                "<< /Type /StructElem /S /P /P 5 0 R >>",
            ],
        );
        let document = &d.struct_tree().unwrap().children[0];
        assert_eq!(document.child_elements().count(), 1);
        assert_eq!(
            document.child_elements().next().unwrap().role,
            StructRole::P
        );
    }

    #[test]
    fn cyclic_kids_terminate() {
        // Element 5 /K -> 6, element 6 /K -> 5: the visited guard stops the loop.
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /Document /K 6 0 R >>",
                "<< /Type /StructElem /S /Sect /K 5 0 R >>",
            ],
        );
        let tree = d.struct_tree().expect("tree (no hang)");
        // Document -> Sect, and Sect's back-edge to Document is cut.
        assert_eq!(tree.children.len(), 1);
        let sect = tree.children[0].child_elements().next().unwrap();
        assert_eq!(sect.role, StructRole::Sect);
        assert_eq!(sect.child_elements().count(), 0);
    }

    #[test]
    fn root_back_edge_makes_no_spurious_element() {
        // The top element's only kid points back at the StructTreeRoot ref, which
        // is pre-seeded into the visited set.
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                "<< /Type /StructElem /S /Document /K 4 0 R >>",
            ],
        );
        let document = &d.struct_tree().unwrap().children[0];
        assert_eq!(document.role, StructRole::Document);
        assert_eq!(document.child_elements().count(), 0, "root back-edge cut");
    }

    #[test]
    fn role_map_cycle_terminates() {
        // /RoleMap maps Foo -> Bar -> Foo; resolution must not loop, and the type
        // stays non-standard (Other).
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R /RoleMap << /Foo /Bar /Bar /Foo >> >>",
                "<< /Type /StructElem /S /Foo /P 4 0 R >>",
            ],
        );
        let role = &d.struct_tree().expect("tree (no hang)").children[0].role;
        assert!(!role.is_standard());
    }

    #[test]
    fn deeply_nested_tree_terminates() {
        // A chain of elements deeper than MAX_STRUCT_DEPTH must terminate without
        // a stack overflow; the over-deep tail is pruned.
        let depth = MAX_STRUCT_DEPTH + 50;
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>".to_string(),
            PAGES.to_string(),
            PAGE.to_string(),
            "<< /Type /StructTreeRoot /K 5 0 R >>".to_string(),
        ];
        // Objects 5..(5+depth): each /K points at the next; the last has none.
        for i in 0..depth {
            let obj_num = 5 + i;
            if i + 1 < depth {
                objs.push(format!(
                    "<< /Type /StructElem /S /Div /K {} 0 R >>",
                    obj_num + 1
                ));
            } else {
                objs.push("<< /Type /StructElem /S /Div >>".to_string());
            }
        }
        let refs: Vec<&str> = objs.iter().map(|s| s.as_str()).collect();
        let d = open(&refs);
        // No panic / no hang; the tree is built up to the depth cap.
        assert!(d.struct_tree().is_some());
    }

    #[test]
    fn huge_alt_text_is_capped() {
        let big = "A".repeat(MAX_TEXT_CHARS + 1000);
        let d = doc(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R >>",
            &[
                "<< /Type /StructTreeRoot /K 5 0 R >>",
                &format!("<< /Type /StructElem /S /Figure /Alt ({big}) >>"),
            ],
        );
        let alt = d.struct_tree().unwrap().children[0].alt.clone().unwrap();
        assert_eq!(alt.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn role_name_round_trip() {
        for name in [
            "Document", "TOC", "TOCI", "P", "H1", "H6", "L", "LI", "Lbl", "LBody", "Table", "TR",
            "TH", "TD", "THead", "TBody", "TFoot", "Span", "BibEntry", "Link", "RB", "WP",
            "Figure", "Formula", "Form",
        ] {
            let role = StructRole::from_name(name);
            assert!(role.is_standard(), "{name} should be standard");
            assert_eq!(role.as_str(), name, "round-trip for {name}");
        }
    }
}
