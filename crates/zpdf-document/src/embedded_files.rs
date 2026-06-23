//! Embedded files and associated files (ISO 32000-1 §7.11, ISO 32000-2 §7.11.4).
//!
//! Two related PDF features surface here through one [`EmbeddedFile`] model:
//!
//! * **Embedded files** — file streams stored inside the PDF and registered in
//!   the catalog's `/Names /EmbeddedFiles` *name tree* (the "attachments" panel
//!   of a viewer; ISO 32000-1). Each name maps to a *file specification*
//!   dictionary whose `/EF` entry points at the embedded-file stream.
//!
//! * **Associated files (`/AF`)** — a PDF 2.0 addition: an array of file
//!   specifications attached to the catalog, a page, an annotation, an XObject,
//!   etc., each carrying an `/AFRelationship` that states *why* the file is
//!   attached (`/Source`, `/Data`, `/Alternative`, …). This is the mechanism
//!   PDF/A-3 and ZUGFeRD/Factur-X use to embed the source XML of an invoice.
//!   PDF 2.0 requires every associated file to *also* appear in the
//!   `/Names /EmbeddedFiles` tree, so the two lists usually overlap.
//!
//! This module only *parses and exposes* metadata and the embedded stream's
//! object id; it never decodes the (potentially large) payload. Callers pull the
//! bytes on demand via [`crate::PdfDocument::embedded_file_bytes`], which routes
//! through the parser's filter pipeline (and so respects `ParseLimits`).

use std::collections::HashSet;

use zpdf_core::{ObjectId, PdfDict, PdfObject};
use zpdf_parser::PdfFile;

use crate::forms::pdf_string_to_unicode;

/// Maximum depth of a `/Names /EmbeddedFiles` name-tree walk. Real trees are a
/// handful of levels; this only bounds adversarial input (in concert with the
/// visited-set cycle guard).
const MAX_NAME_TREE_DEPTH: usize = 64;

/// Defensive cap on the total number of embedded-file entries collected from one
/// name tree — bounds a maliciously enormous (or cyclic-but-distinct) tree.
const MAX_EMBEDDED_FILES: usize = 16_384;

/// Defensive cap on the number of entries read from one `/AF` array.
const MAX_AF_ENTRIES: usize = 8_192;

/// File-name keys on a file-specification dictionary, in preference order:
/// the Unicode name (`/UF`, PDF 1.7) first, then the platform-independent `/F`,
/// then the legacy platform-specific names. The same order picks the embedded
/// stream out of an `/EF` dictionary.
const FILE_NAME_KEYS: [&str; 5] = ["UF", "F", "Unix", "DOS", "Mac"];

/// Where an [`EmbeddedFile`] was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbeddedSource {
    /// The catalog's `/Names /EmbeddedFiles` name tree (document attachments).
    NameTree,
    /// An `/AF` associated-files array (PDF 2.0). The semantic relationship is
    /// carried separately in [`EmbeddedFile::relationship`]; the owning scope
    /// (catalog vs. page) is implied by which accessor returned it.
    AssociatedFile,
}

/// One embedded or associated file: the file-specification metadata plus the
/// object id of its embedded-file stream (when it carries one). The payload is
/// not decoded here — fetch it with [`crate::PdfDocument::embedded_file_bytes`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmbeddedFile {
    /// Best available file name: `/UF` (Unicode) if present, else `/F`, else a
    /// platform-specific name, else the name-tree key it was registered under.
    /// May be empty if the file specification carries no name at all.
    pub name: String,
    /// `/Desc` — a human-readable description, if present.
    pub description: Option<String>,
    /// `/AFRelationship` (PDF 2.0): the relationship an associated file has to
    /// the content it is attached to — `Source`, `Data`, `Alternative`,
    /// `Supplement`, `EncryptedPayload`, `FormData`, `Schema`, `Unspecified`.
    /// `None` when absent (the common case for plain name-tree attachments).
    pub relationship: Option<String>,
    /// `/Subtype` of the embedded-file stream — a MIME type such as
    /// `"application/xml"` (the PDF name `application#2Fxml`). `None` when the
    /// file carries no embedded stream or the stream omits `/Subtype`.
    pub subtype: Option<String>,
    /// `/Params /Size` — the uncompressed size in bytes the producer declared.
    /// Advisory: the actual decoded length is whatever the stream yields.
    pub size: Option<i64>,
    /// `/Params /CreationDate`, as the raw PDF date string (e.g. `D:20240101…`).
    pub creation_date: Option<String>,
    /// `/Params /ModDate`, as the raw PDF date string.
    pub mod_date: Option<String>,
    /// `/Params /CheckSum` — a 16-byte MD5 of the *uncompressed* bytes, if the
    /// producer included one. Stored raw (not decoded text).
    pub checksum: Option<Vec<u8>>,
    /// Object id of the embedded-file stream (`/EF` → chosen name key). `None`
    /// for an external file reference or a malformed spec with no `/EF`.
    pub stream: Option<ObjectId>,
    /// Whether this came from the name tree or an `/AF` array.
    pub source: EmbeddedSource,
}

impl EmbeddedFile {
    /// Whether this file specification actually has embedded bytes to extract
    /// (as opposed to merely naming an external file).
    pub fn is_embedded(&self) -> bool {
        self.stream.is_some()
    }
}

/// Document-level embedded files from the catalog's `/Names /EmbeddedFiles`
/// name tree. Empty when the document declares none.
pub fn parse_embedded_files(file: &PdfFile) -> Vec<EmbeddedFile> {
    let Some(root) = catalog_dict(file) else {
        return Vec::new();
    };
    // /Root /Names is a dictionary of name trees (/EmbeddedFiles, /Dests, …).
    let Some(names) = resolve_dict(file, root.get("Names")) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut visited = HashSet::new();
    // Seed the cycle guard with the tree-root reference itself so a root that
    // lists itself as a kid terminates.
    if let Some(PdfObject::Ref(r)) = names.get("EmbeddedFiles") {
        visited.insert(*r);
    }
    let Some(tree_root) = resolve_dict(file, names.get("EmbeddedFiles")) else {
        return Vec::new();
    };
    walk_name_tree(file, &tree_root, &mut out, &mut visited, 0);
    out
}

/// Catalog-level associated files (`/Root /AF`, PDF 2.0). Empty for most files.
pub fn parse_associated_files(file: &PdfFile) -> Vec<EmbeddedFile> {
    let Some(root) = catalog_dict(file) else {
        return Vec::new();
    };
    collect_af_array(file, root.get("AF"))
}

/// Page-level associated files (`/Page /AF`, PDF 2.0), read off an
/// already-resolved leaf page dictionary. `/AF` is *not* an inheritable page
/// attribute, so this looks only at the leaf.
pub fn parse_page_associated_files(file: &PdfFile, page_dict: &PdfDict) -> Vec<EmbeddedFile> {
    collect_af_array(file, page_dict.get("AF"))
}

/// Walk a name-tree node, appending embedded-file entries. A leaf node carries
/// `/Names [key0 val0 key1 val1 …]`; an interior node carries `/Kids [refs]`.
/// Bounded by depth, a per-reference visited set, and the global entry cap.
fn walk_name_tree(
    file: &PdfFile,
    node: &PdfDict,
    out: &mut Vec<EmbeddedFile>,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) {
    if depth > MAX_NAME_TREE_DEPTH || out.len() >= MAX_EMBEDDED_FILES {
        return;
    }

    // Leaf entries: alternating (name-string, file-specification) pairs.
    if let Some(names) = resolve_array(file, node.get("Names")) {
        let mut i = 0;
        while i + 1 < names.len() {
            if out.len() >= MAX_EMBEDDED_FILES {
                return;
            }
            let key = match &names[i] {
                PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
                _ => None,
            };
            if let Some(ef) = parse_file_spec(file, &names[i + 1], key, EmbeddedSource::NameTree) {
                out.push(ef);
            }
            i += 2;
        }
    }

    // Interior children.
    if let Some(kids) = resolve_array(file, node.get("Kids")) {
        for kid in &kids {
            let kid_dict = match kid {
                PdfObject::Ref(r) => {
                    // Cycle guard: only descend through each node once.
                    if !visited.insert(*r) {
                        continue;
                    }
                    resolve_dict(file, Some(kid))
                }
                PdfObject::Dict(_) => resolve_dict(file, Some(kid)),
                _ => None,
            };
            if let Some(d) = kid_dict {
                walk_name_tree(file, &d, out, visited, depth + 1);
            }
        }
    }
}

/// Parse the entries of an `/AF` array (each a file specification, possibly an
/// indirect reference). Bounded by [`MAX_AF_ENTRIES`].
fn collect_af_array(file: &PdfFile, obj: Option<&PdfObject>) -> Vec<EmbeddedFile> {
    let Some(arr) = resolve_array(file, obj) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for elem in arr.iter().take(MAX_AF_ENTRIES) {
        if let Some(ef) = parse_file_spec(file, elem, None, EmbeddedSource::AssociatedFile) {
            out.push(ef);
        }
    }
    out
}

/// Resolve a value to a file specification and extract its metadata. The value
/// is usually an indirect reference to a `/Filespec` dictionary; a bare string
/// is a *simple* file specification (an external path, no embedded stream).
/// `tree_key` is the name-tree key, used as a fallback file name.
fn parse_file_spec(
    file: &PdfFile,
    obj: &PdfObject,
    tree_key: Option<String>,
    source: EmbeddedSource,
) -> Option<EmbeddedFile> {
    let resolved = match obj {
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        other => other.clone(),
    };
    match resolved {
        PdfObject::Dict(d) => Some(parse_file_spec_dict(file, &d, tree_key, source)),
        PdfObject::String(s) => {
            // Simple (external) file specification: a path string, no payload.
            let name = non_empty(pdf_string_to_unicode(s.as_bytes()))
                .or(tree_key)
                .unwrap_or_default();
            Some(EmbeddedFile {
                name,
                description: None,
                relationship: None,
                subtype: None,
                size: None,
                creation_date: None,
                mod_date: None,
                checksum: None,
                stream: None,
                source,
            })
        }
        _ => None,
    }
}

fn parse_file_spec_dict(
    file: &PdfFile,
    dict: &PdfDict,
    tree_key: Option<String>,
    source: EmbeddedSource,
) -> EmbeddedFile {
    let name = file_spec_name(file, dict).or(tree_key).unwrap_or_default();
    let description = text(file, dict, "Desc");
    let relationship = name_value(file, dict, "AFRelationship");

    // /EF maps name keys → embedded-file stream references. Pick the stream by
    // the same preference order as the file name.
    let stream = resolve_dict(file, dict.get("EF")).and_then(|ef| pick_ef_stream(&ef));

    // Read /Subtype and /Params off the stream *dictionary* without decoding the
    // payload — enough for a listing. Each value may be an indirect reference.
    let mut subtype = None;
    let mut size = None;
    let mut creation_date = None;
    let mut mod_date = None;
    let mut checksum = None;
    if let Some(stream_dict) = stream
        .and_then(|id| file.resolve(id).ok())
        .and_then(|o| o.as_stream().ok().map(|s| s.dict.clone()))
    {
        subtype = name_value(file, &stream_dict, "Subtype");
        if let Some(params) = resolve_dict(file, stream_dict.get("Params")) {
            size = integer(file, &params, "Size");
            creation_date = text(file, &params, "CreationDate");
            mod_date = text(file, &params, "ModDate");
            checksum = string_bytes(file, &params, "CheckSum");
        }
    }

    EmbeddedFile {
        name,
        description,
        relationship,
        subtype,
        size,
        creation_date,
        mod_date,
        checksum,
        stream,
        source,
    }
}

/// First non-empty file name on a file-specification dict, in `/UF`,`/F`,…
/// preference order. Each name may itself be an indirect string reference.
fn file_spec_name(file: &PdfFile, dict: &PdfDict) -> Option<String> {
    FILE_NAME_KEYS
        .iter()
        .find_map(|k| text(file, dict, k).and_then(non_empty))
}

/// The embedded-file stream reference from an `/EF` dictionary, by preference
/// order. (Stream objects are always indirect, so these are references.)
fn pick_ef_stream(ef: &PdfDict) -> Option<ObjectId> {
    FILE_NAME_KEYS.iter().find_map(|k| ef.get_ref(k).ok())
}

/// The catalog dictionary (`/Root`), or `None` if unreachable.
fn catalog_dict(file: &PdfFile) -> Option<PdfDict> {
    let root_ref = file.trailer.get_ref("Root").ok()?;
    file.resolve(root_ref).ok()?.as_dict().ok().cloned()
}

/// Resolve a dictionary value that may be given directly or indirectly.
fn resolve_dict(file: &PdfFile, obj: Option<&PdfObject>) -> Option<PdfDict> {
    match obj? {
        PdfObject::Dict(d) => Some(d.clone()),
        // A stream object also satisfies dictionary lookups for the node / /EF /
        // /Params positions, which a lax producer may emit as a stream; expose
        // its dictionary. (The file-spec value itself goes through
        // `parse_file_spec`, not here.)
        PdfObject::Stream(s) => Some(s.dict.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Dict(d) => Some(d),
            PdfObject::Stream(s) => Some(s.dict),
            _ => None,
        },
        _ => None,
    }
}

/// Resolve an array value that may be given directly or indirectly.
fn resolve_array(file: &PdfFile, obj: Option<&PdfObject>) -> Option<Vec<PdfObject>> {
    match obj? {
        PdfObject::Array(a) => Some(a.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Array(a) => Some(a),
            _ => None,
        },
        _ => None,
    }
}

/// Decode a text-string dict entry (UTF-16BE with BOM, else PDFDocEncoding),
/// following one indirect reference.
fn text(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<String> {
    let value = match dict.get(key)? {
        PdfObject::String(s) => return Some(pdf_string_to_unicode(s.as_bytes())),
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        _ => return None,
    };
    match value {
        PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
        _ => None,
    }
}

/// Read a Name-valued dict entry, following one indirect reference. (Any object
/// value may be written indirectly, so `/Subtype 9 0 R` and `/AFRelationship
/// 9 0 R` must resolve rather than drop.)
fn name_value(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<String> {
    let value = match dict.get(key)? {
        PdfObject::Name(n) => return Some(n.as_str().to_string()),
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        _ => return None,
    };
    match value {
        PdfObject::Name(n) => Some(n.as_str().to_string()),
        _ => None,
    }
}

/// Read an integer-valued dict entry, following one indirect reference and
/// accepting a whole-valued Real (some producers write `/Size 42.0`).
fn integer(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<i64> {
    let value = match dict.get(key)? {
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        other => other.clone(),
    };
    match value {
        PdfObject::Integer(n) => Some(n),
        PdfObject::Real(r) if r.is_finite() && r.fract() == 0.0 => Some(r as i64),
        _ => None,
    }
}

/// Raw bytes of a string-valued dict entry (e.g. `/CheckSum`), following one
/// indirect reference. Not text-decoded — a checksum is binary.
fn string_bytes(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<Vec<u8>> {
    let value = match dict.get(key)? {
        PdfObject::String(s) => return Some(s.0.clone()),
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        _ => return None,
    };
    match value {
        PdfObject::String(s) => Some(s.0),
        _ => None,
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
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

    // A minimal page tree shared by the fixtures (objects 2 and 3).
    const PAGES: &str = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    const PAGE: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";

    #[test]
    fn name_tree_single_leaf_with_stream() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (hello.txt) 5 0 R ] >>",
            "<< /Type /Filespec /F (hello.txt) /UF (hello.txt) /Desc (greeting) \
             /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Subtype /text#2Fplain /Params << /Size 5 >> /Length 5 >>\n\
             stream\nHello\nendstream",
        ]);
        let efs = doc.embedded_files();
        assert_eq!(efs.len(), 1);
        let ef = &efs[0];
        assert_eq!(ef.name, "hello.txt");
        assert_eq!(ef.description.as_deref(), Some("greeting"));
        assert_eq!(ef.subtype.as_deref(), Some("text/plain"));
        assert_eq!(ef.size, Some(5));
        assert_eq!(ef.stream, Some(ObjectId(6, 0)));
        assert_eq!(ef.source, EmbeddedSource::NameTree);
        assert!(ef.is_embedded());
        // Payload extraction round-trips through the filter pipeline.
        let bytes = doc.embedded_file_bytes(ef).expect("extract");
        assert_eq!(bytes, b"Hello");
    }

    #[test]
    fn name_tree_interior_kids_node() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Kids [5 0 R] >>",
            "<< /Limits [(a.txt) (a.txt)] /Names [ (a.txt) 6 0 R ] >>",
            "<< /Type /Filespec /UF (a.txt) /EF << /F 7 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 1 >>\nstream\nx\nendstream",
        ]);
        let efs = doc.embedded_files();
        assert_eq!(efs.len(), 1);
        assert_eq!(efs[0].name, "a.txt");
    }

    #[test]
    fn associated_file_with_relationship() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /AF [4 0 R] >>",
            PAGES,
            PAGE,
            "<< /Type /Filespec /F (invoice.xml) /UF (invoice.xml) \
             /AFRelationship /Data /EF << /F 5 0 R >> >>",
            "<< /Type /EmbeddedFile /Subtype /application#2Fxml /Length 7 >>\n\
             stream\n<x></x>\nendstream",
        ]);
        let afs = doc.associated_files();
        assert_eq!(afs.len(), 1);
        assert_eq!(afs[0].name, "invoice.xml");
        assert_eq!(afs[0].relationship.as_deref(), Some("Data"));
        assert_eq!(afs[0].subtype.as_deref(), Some("application/xml"));
        assert_eq!(afs[0].source, EmbeddedSource::AssociatedFile);
        assert!(
            doc.embedded_files().is_empty(),
            "AF is not in the name tree here"
        );
    }

    #[test]
    fn page_level_associated_file() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            PAGES,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /AF [4 0 R] >>",
            "<< /Type /Filespec /F (page-data.bin) /AFRelationship /Supplement \
             /EF << /F 5 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        let page = doc.page(0).expect("page");
        let afs = doc.page_associated_files(&page);
        assert_eq!(afs.len(), 1);
        assert_eq!(afs[0].name, "page-data.bin");
        assert_eq!(afs[0].relationship.as_deref(), Some("Supplement"));
        // Catalog-level AF is empty for this document.
        assert!(doc.associated_files().is_empty());
    }

    #[test]
    fn utf16be_unicode_name_decodes() {
        // /UF <FEFF 0066 0069 006C 0065 002E 0074 0078 0074> = "file.txt".
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (k) 5 0 R ] >>",
            "<< /Type /Filespec /F (fallback.txt) \
             /UF <FEFF00660069006C0065002E007400780074> /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        let efs = doc.embedded_files();
        assert_eq!(efs.len(), 1);
        // /UF is preferred over /F.
        assert_eq!(efs[0].name, "file.txt");
    }

    #[test]
    fn name_tree_self_cycle_terminates() {
        // The tree root lists itself as a kid; the walk must terminate.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Kids [4 0 R] /Names [ (a) 5 0 R ] >>",
            "<< /Type /Filespec /UF (a) /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        let efs = doc.embedded_files();
        // The single leaf entry is collected exactly once; no hang.
        assert_eq!(efs.len(), 1);
    }

    #[test]
    fn filespec_without_ef_is_listed_but_not_embedded() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (ext) 5 0 R ] >>",
            "<< /Type /Filespec /F (external.txt) >>",
        ]);
        let efs = doc.embedded_files();
        assert_eq!(efs.len(), 1);
        assert_eq!(efs[0].name, "external.txt");
        assert_eq!(efs[0].stream, None);
        assert!(!efs[0].is_embedded());
        // Asking for bytes on a non-embedded spec is an error, not a panic.
        assert!(doc.embedded_file_bytes(&efs[0]).is_err());
    }

    #[test]
    fn no_names_dict_is_empty() {
        let doc = open(&["<< /Type /Catalog /Pages 2 0 R >>", PAGES, PAGE]);
        assert!(doc.embedded_files().is_empty());
        assert!(doc.associated_files().is_empty());
    }

    #[test]
    fn params_dates_and_checksum_parsed() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (d) 5 0 R ] >>",
            "<< /Type /Filespec /UF (d.bin) /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 \
             /Params << /Size 42 /CreationDate (D:20240101000000Z) /ModDate (D:20240102000000Z) \
             /CheckSum <00112233445566778899aabbccddeeff> >> >>\nstream\n\nendstream",
        ]);
        let ef = &doc.embedded_files()[0];
        assert_eq!(ef.size, Some(42));
        assert_eq!(ef.creation_date.as_deref(), Some("D:20240101000000Z"));
        assert_eq!(ef.mod_date.as_deref(), Some("D:20240102000000Z"));
        assert_eq!(ef.checksum.as_ref().map(|c| c.len()), Some(16));
    }

    #[test]
    fn multi_pair_leaf_collects_all_in_order() {
        // The core walker loop: a single leaf with three (key, filespec) pairs.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (a) 5 0 R (b) 6 0 R (c) 7 0 R ] >>",
            "<< /Type /Filespec /UF (a.txt) /EF << /F 8 0 R >> >>",
            "<< /Type /Filespec /UF (b.txt) /EF << /F 8 0 R >> >>",
            "<< /Type /Filespec /UF (c.txt) /EF << /F 8 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        let names: Vec<_> = doc.embedded_files().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn two_sibling_kids_each_contribute() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Kids [5 0 R 6 0 R] >>",
            "<< /Names [ (a) 7 0 R ] >>",
            "<< /Names [ (b) 8 0 R ] >>",
            "<< /Type /Filespec /UF (a.txt) /EF << /F 9 0 R >> >>",
            "<< /Type /Filespec /UF (b.txt) /EF << /F 9 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        assert_eq!(doc.embedded_files().len(), 2);
    }

    #[test]
    fn odd_length_names_drops_trailing_key() {
        // A dangling trailing key (odd-length /Names) is dropped, not paired or panicked.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (a) 5 0 R (orphan) ] >>",
            "<< /Type /Filespec /UF (a.txt) /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        let efs = doc.embedded_files();
        assert_eq!(efs.len(), 1);
        assert_eq!(efs[0].name, "a.txt");
    }

    #[test]
    fn inline_dict_filespec_value() {
        // The name-tree value is a DIRECT Filespec dict, not an indirect ref.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (x) << /Type /Filespec /UF (x.txt) /EF << /F 5 0 R >> >> ] >>",
            "<< /Type /EmbeddedFile /Length 2 >>\nstream\nhi\nendstream",
        ]);
        let efs = doc.embedded_files();
        assert_eq!(efs.len(), 1);
        assert_eq!(efs[0].name, "x.txt");
        assert_eq!(efs[0].stream, Some(ObjectId(5, 0)));
        assert_eq!(doc.embedded_file_bytes(&efs[0]).expect("bytes"), b"hi");
    }

    #[test]
    fn inline_dict_kid_node() {
        // An interior /Kids entry that is a direct (inline) leaf dict.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Kids [ << /Names [ (a.txt) 5 0 R ] >> ] >>",
            "<< /Type /Filespec /UF (a.txt) /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        assert_eq!(doc.embedded_files().len(), 1);
    }

    #[test]
    fn bare_string_external_af_has_no_stream() {
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /AF [ (../external.dat) ] >>",
            PAGES,
            PAGE,
        ]);
        let afs = doc.associated_files();
        assert_eq!(afs.len(), 1);
        assert_eq!(afs[0].name, "../external.dat");
        assert_eq!(afs[0].stream, None);
        assert!(!afs[0].is_embedded());
        assert!(doc.embedded_file_bytes(&afs[0]).is_err());
    }

    #[test]
    fn tree_key_is_name_fallback_when_filespec_unnamed() {
        // The Filespec carries no /UF or /F; the name-tree key supplies the name.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (keyname.txt) 5 0 R ] >>",
            "<< /Type /Filespec /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        assert_eq!(doc.embedded_files()[0].name, "keyname.txt");
    }

    #[test]
    fn names_dict_without_embeddedfiles_is_empty() {
        // /Names present but with a sibling tree (no /EmbeddedFiles) — distinct
        // exit from "no /Names at all".
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (foo) (bar) ] >>",
        ]);
        assert!(doc.embedded_files().is_empty());
        assert!(doc.associated_files().is_empty());
    }

    #[test]
    fn indirect_uf_name_and_desc_resolve() {
        // /UF, /F, and /Desc given as indirect string references must resolve.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (k) 5 0 R ] >>",
            "<< /Type /Filespec /UF 7 0 R /F (fallback.txt) /Desc 8 0 R /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
            "(indirect.txt)",
            "(indirect desc)",
        ]);
        let ef = &doc.embedded_files()[0];
        assert_eq!(ef.name, "indirect.txt"); // indirect /UF wins over direct /F
        assert_eq!(ef.description.as_deref(), Some("indirect desc"));
    }

    #[test]
    fn indirect_subtype_and_relationship_resolve() {
        // /AFRelationship and /Subtype given as indirect name references resolve.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /AF [4 0 R] >>",
            PAGES,
            PAGE,
            "<< /Type /Filespec /F (i.xml) /AFRelationship 6 0 R /EF << /F 5 0 R >> >>",
            "<< /Type /EmbeddedFile /Subtype 7 0 R /Length 0 >>\nstream\n\nendstream",
            "/Data",
            "/application#2Fxml",
        ]);
        let af = &doc.associated_files()[0];
        assert_eq!(af.relationship.as_deref(), Some("Data"));
        assert_eq!(af.subtype.as_deref(), Some("application/xml"));
    }

    #[test]
    fn params_size_accepts_real_and_indirect() {
        // Whole-valued Real /Size.
        let real = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (r) 5 0 R ] >>",
            "<< /Type /Filespec /UF (r.bin) /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 /Params << /Size 42.0 >> >>\nstream\n\nendstream",
        ]);
        assert_eq!(real.embedded_files()[0].size, Some(42));

        // Indirect integer /Size.
        let indirect = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (r) 5 0 R ] >>",
            "<< /Type /Filespec /UF (r.bin) /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 /Params << /Size 7 0 R >> >>\nstream\n\nendstream",
            "99",
        ]);
        assert_eq!(indirect.embedded_files()[0].size, Some(99));
    }

    #[test]
    fn name_tree_root_is_a_stream_object() {
        // A lax producer makes the tree-root node a stream whose dict carries
        // /Names — resolve_dict exposes the stream's dict so the walk proceeds.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Names [ (s.bin) 5 0 R ] /Length 0 >>\nstream\n\nendstream",
            "<< /Type /Filespec /UF (s.bin) /EF << /F 6 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 1 >>\nstream\nx\nendstream",
        ]);
        let efs = doc.embedded_files();
        assert_eq!(efs.len(), 1);
        assert_eq!(efs[0].name, "s.bin");
        assert_eq!(efs[0].stream, Some(ObjectId(6, 0)));
    }

    #[test]
    fn mid_tree_reference_cycle_terminates() {
        // A back-edge deep in the tree (obj6 → obj5) must be cut by the visited set.
        let doc = open(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
            PAGES,
            PAGE,
            "<< /Kids [5 0 R] >>",
            "<< /Kids [6 0 R] >>",
            "<< /Kids [5 0 R] /Names [ (a) 7 0 R ] >>",
            "<< /Type /Filespec /UF (a.txt) /EF << /F 8 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]);
        assert_eq!(doc.embedded_files().len(), 1);
    }

    #[test]
    fn over_deep_name_tree_is_pruned() {
        // A /Kids chain deeper than the depth cap: the leaf below it is never
        // reached and the walk returns (no stack overflow / hang).
        let depth = MAX_NAME_TREE_DEPTH + 5;
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>".into(),
            PAGES.into(),
            PAGE.into(),
        ];
        for k in 0..depth {
            objs.push(format!("<< /Kids [{} 0 R] >>", 4 + k + 1));
        }
        let leaf = 4 + depth;
        objs.push(format!("<< /Names [ (deep) {} 0 R ] >>", leaf + 1));
        objs.push(format!(
            "<< /Type /Filespec /UF (deep) /EF << /F {} 0 R >> >>",
            leaf + 2
        ));
        objs.push("<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream".into());
        let refs: Vec<&str> = objs.iter().map(|s| s.as_str()).collect();
        let doc = PdfDocument::open(build_pdf(&refs)).expect("open");
        assert!(doc.embedded_files().is_empty());
    }
}
