//! XMP document metadata (ISO 32000-1 §14.3.2 / Adobe XMP). The catalog's
//! `/Metadata` entry is a stream carrying an XMP packet — RDF/XML describing the
//! document with Dublin Core (`dc:`), XMP Basic (`xmp:`), and PDF-schema (`pdf:`)
//! properties. PDF 2.0 deprecates the `/Info` dictionary in favour of this, so
//! XMP is increasingly the only place some metadata lives.
//!
//! **This reads the common properties with a bounded tag/attribute *scrape*, not
//! a full XML parser.** That is deliberate: an XML engine that resolves general
//! entities is vulnerable to "billion laughs" entity-expansion bombs, and a DOM
//! builder can blow the stack on deeply-nested input. Here no general entity is
//! ever resolved (only the five predefined XML entities and numeric character
//! references, each of which maps to exactly one character), every scan is
//! linear over the byte string, and every field and array is length-capped. Like
//! the other navigation/metadata readers it runs only when explicitly called —
//! never during `open` or rendering.
//!
//! The trade-off: a producer that binds the schema namespaces to non-standard
//! prefixes (not `dc`/`xmp`/`pdf`) is not recognized. In practice these prefixes
//! are universal.

use std::borrow::Cow;

use zpdf_core::PdfObject;
use zpdf_parser::PdfFile;

use crate::obj_util::catalog_dict;

/// Upper bound on XMP packet bytes scanned. XMP packets are typically far under
/// 100 KiB; this caps a pathological `/Metadata` stream.
const MAX_XMP_BYTES: usize = 8 * 1024 * 1024;
/// Per-field character cap (after entity decoding). Bounds an adversarial value.
const MAX_FIELD_LEN: usize = 8192;
/// Cap on `rdf:li` items collected from one array/alt property.
const MAX_LI: usize = 1024;

/// Common document metadata read from the XMP packet. Every field is optional;
/// producers populate an arbitrary subset, and a field may be present in XMP but
/// not in `/Info` (or vice-versa).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmpMetadata {
    /// `dc:title` — the document title (the `x-default` language alternative).
    pub title: Option<String>,
    /// `dc:creator` — the author(s), in order.
    pub creators: Vec<String>,
    /// `dc:description` — a description/abstract (the `x-default` alternative).
    pub description: Option<String>,
    /// `dc:subject` — subject keywords/phrases.
    pub subjects: Vec<String>,
    /// `pdf:Keywords` — the keyword string (the PDF-schema simple property).
    pub keywords: Option<String>,
    /// `pdf:Producer` — the application that produced the PDF.
    pub producer: Option<String>,
    /// `xmp:CreatorTool` — the application that authored the original document.
    pub creator_tool: Option<String>,
    /// `xmp:CreateDate` — creation timestamp (raw XMP/ISO-8601 date string).
    pub create_date: Option<String>,
    /// `xmp:ModifyDate` — last-modification timestamp (raw date string).
    pub modify_date: Option<String>,
}

impl XmpMetadata {
    /// Whether every field is absent.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.creators.is_empty()
            && self.description.is_none()
            && self.subjects.is_empty()
            && self.keywords.is_none()
            && self.producer.is_none()
            && self.creator_tool.is_none()
            && self.create_date.is_none()
            && self.modify_date.is_none()
    }
}

/// Decode and return the raw bytes of the catalog's `/Metadata` XMP stream, or
/// `None` when the document carries none. Routes through the parser's filter
/// pipeline, so it respects `ParseLimits`.
pub fn metadata_bytes(file: &PdfFile) -> Option<Vec<u8>> {
    let root = catalog_dict(file)?;
    // /Metadata is an indirect reference to a stream (streams are always indirect
    // objects in PDF), so a Ref is the only valid shape.
    let id = match root.get("Metadata")? {
        PdfObject::Ref(r) => *r,
        _ => return None,
    };
    file.resolve_stream_data(id).ok()
}

/// Parse the catalog's XMP `/Metadata` packet into [`XmpMetadata`]. Returns
/// `None` when the document carries no `/Metadata`, it cannot be decoded, or it
/// holds none of the recognized properties.
pub fn parse_xmp(file: &PdfFile) -> Option<XmpMetadata> {
    let bytes = metadata_bytes(file)?;
    let xml = decode_text(&bytes);
    let meta = scrape(&xml);
    if meta.is_empty() {
        None
    } else {
        Some(meta)
    }
}

/// Decode the XMP packet bytes to text. XMP is UTF-8 by convention but may carry
/// a UTF-8 or UTF-16 byte-order mark; honour the BOM, else assume UTF-8. The
/// input is capped to [`MAX_XMP_BYTES`] first.
fn decode_text(bytes: &[u8]) -> String {
    let bytes = &bytes[..bytes.len().min(MAX_XMP_BYTES)];
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        decode_utf16(rest, true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        decode_utf16(rest, false)
    } else {
        let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        String::from_utf8_lossy(rest).into_owned()
    }
}

/// Decode UTF-16 (big- or little-endian) bytes leniently.
fn decode_utf16(bytes: &[u8], be: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| {
            if be {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Scrape the recognized properties out of the XMP text. Comments are stripped
/// first, so a property commented out before the real one is not matched.
fn scrape(xml: &str) -> XmpMetadata {
    let xml = strip_comments(xml);
    XmpMetadata {
        title: alt_property(&xml, "dc:title"),
        creators: array_property(&xml, "dc:creator"),
        description: alt_property(&xml, "dc:description"),
        subjects: array_property(&xml, "dc:subject"),
        keywords: simple_property(&xml, "pdf:Keywords"),
        producer: simple_property(&xml, "pdf:Producer"),
        creator_tool: simple_property(&xml, "xmp:CreatorTool"),
        create_date: simple_property(&xml, "xmp:CreateDate"),
        modify_date: simple_property(&xml, "xmp:ModifyDate"),
    }
}

/// Remove `<!-- … -->` comment spans so a commented-out property can't be
/// matched as the real value. Linear and bounded; an unterminated comment is
/// dropped to the end of the input. Borrows unchanged input when there are no
/// comments (the common case).
fn strip_comments(xml: &str) -> Cow<'_, str> {
    if !xml.contains("<!--") {
        return Cow::Borrowed(xml);
    }
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        match after.find("-->") {
            Some(end) => rest = &after[end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// A simple (text) property: the element's text, else the RDF attribute form.
fn simple_property(xml: &str, qname: &str) -> Option<String> {
    element_inner(xml, qname)
        .and_then(simple_text)
        .or_else(|| attribute_value(xml, qname))
}

/// A language-alternative property (`rdf:Alt`): the `x-default` item, else the
/// first item, else (degenerate) the element's text or the attribute form.
fn alt_property(xml: &str, qname: &str) -> Option<String> {
    if let Some(inner) = element_inner(xml, qname) {
        let lis = scan_li(inner);
        if let Some(pick) = lis.iter().find(|l| l.x_default).or_else(|| lis.first()) {
            return Some(pick.value.clone());
        }
        if let Some(t) = simple_text(inner) {
            return Some(t);
        }
    }
    attribute_value(xml, qname)
}

/// An ordered/unordered array property (`rdf:Seq` / `rdf:Bag`): the `rdf:li`
/// items, else a single value from the element text or attribute form.
fn array_property(xml: &str, qname: &str) -> Vec<String> {
    if let Some(inner) = element_inner(xml, qname) {
        let lis = scan_li(inner);
        if !lis.is_empty() {
            return lis.into_iter().map(|l| l.value).collect();
        }
        if let Some(t) = simple_text(inner) {
            return vec![t];
        }
    }
    attribute_value(xml, qname).into_iter().collect()
}

/// One `rdf:li` item with whether it is the `x-default` language alternative.
struct Li {
    x_default: bool,
    value: String,
}

/// Collect the `rdf:li` items inside an array/alt property's content, bounded by
/// [`MAX_LI`]. Each item's text is entity-decoded and trimmed; empty items are
/// dropped.
fn scan_li(inner: &str) -> Vec<Li> {
    const CLOSE: &str = "</rdf:li>";
    let mut out = Vec::new();
    let mut rest = inner;
    while out.len() < MAX_LI {
        let Some(start) = find_open_tag(rest, "rdf:li") else {
            break;
        };
        let after = &rest[start..];
        let Some(gt) = after.find('>') else {
            break;
        };
        let open = &after[..gt]; // open-tag text (attributes)
        let x_default = open.contains("x-default");
        if open.ends_with('/') {
            // A self-closing (empty) <rdf:li/>; skip past it.
            rest = &after[gt + 1..];
            continue;
        }
        let content_start = gt + 1;
        let Some(close_rel) = after[content_start..].find(CLOSE) else {
            break;
        };
        let value = decode_entities(after[content_start..content_start + close_rel].trim());
        if !value.is_empty() {
            out.push(Li { x_default, value });
        }
        rest = &after[content_start + close_rel + CLOSE.len()..];
    }
    out
}

/// The text content (entity-decoded) of an element's inner span, or `None` when
/// it is empty *or* it is structured (contains child elements). The structured
/// check matters for the array/alt fallback: an empty or unrecognized container
/// like `<rdf:Alt></rdf:Alt>` or `<rdf:Bag/>` must yield `None`, not leak its raw
/// markup as the value. A literal `<` only appears in real text as the `&lt;`
/// entity, so a bare `<` reliably marks markup.
fn simple_text(inner: &str) -> Option<String> {
    let trimmed = inner.trim();
    if trimmed.is_empty() || trimmed.contains('<') {
        return None;
    }
    let t = decode_entities(trimmed);
    (!t.is_empty()).then_some(t)
}

/// The inner content of the first `<qname …>…</qname>` element, or `None`. A
/// self-closing `<qname …/>` yields `Some("")`. No same-name nesting is assumed
/// (XMP properties do not nest a property inside itself).
fn element_inner<'a>(xml: &'a str, qname: &str) -> Option<&'a str> {
    let open = find_open_tag(xml, qname)?;
    let after = &xml[open..];
    let gt = after.find('>')?;
    if after[..gt].ends_with('/') {
        return Some("");
    }
    let content_start = gt + 1;
    let close = format!("</{qname}>");
    let rel = after[content_start..].find(&close)?;
    Some(&after[content_start..content_start + rel])
}

/// Find the byte offset of the first `<qname` opening tag — requiring the next
/// character to be a tag delimiter, so `<dc:title` does not match `<dc:titlebar`.
fn find_open_tag(xml: &str, qname: &str) -> Option<usize> {
    let needle_buf = format!("<{qname}");
    let needle = needle_buf.as_str();
    let mut from = 0;
    while let Some(rel) = xml[from..].find(needle) {
        let pos = from + rel;
        let after_idx = pos + needle.len();
        match xml.as_bytes().get(after_idx) {
            Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/') => return Some(pos),
            None => return None,
            // A longer name that merely starts with `qname`; keep scanning.
            _ => from = after_idx,
        }
    }
    None
}

/// The value of an RDF-shorthand attribute `qname="…"` (or `qname='…'`) on an
/// element such as `rdf:Description`. The match must begin at an attribute
/// boundary so `pdf:Producer` is not found inside a longer attribute name.
fn attribute_value(xml: &str, qname: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = xml[from..].find(qname) {
        let pos = from + rel;
        let prev_ok = pos == 0
            || matches!(
                xml.as_bytes()[pos - 1],
                b' ' | b'\t' | b'\r' | b'\n' | b'<' | b'"' | b'\''
            );
        let after = pos + qname.len();
        if prev_ok {
            let rest = xml[after..].trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                let mut chars = rest.chars();
                if let Some(q @ ('"' | '\'')) = chars.next() {
                    let body = &rest[q.len_utf8()..];
                    if let Some(end) = body.find(q) {
                        return Some(decode_entities(&body[..end]));
                    }
                }
            }
        }
        from = after;
    }
    None
}

/// Decode the five predefined XML entities and numeric character references in a
/// scraped value, capping its length. **No general (DTD-defined) entity is
/// resolved**, so an entity-expansion bomb cannot inflate the output — an
/// unknown `&name;` is left verbatim. Output length ≤ input length ≤
/// [`MAX_FIELD_LEN`].
fn decode_entities(s: &str) -> String {
    let s = cap_len(s);
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // A real entity reference is short; only look a little way for the ';'.
        // Snap the window to a char boundary so a multibyte char straddling the
        // cutoff cannot panic the slice (the value text is arbitrary UTF-8).
        let mut wend = tail.len().min(12);
        while wend > 0 && !tail.is_char_boundary(wend) {
            wend -= 1;
        }
        let window = &tail[..wend];
        if let Some(semi) = window.find(';') {
            if let Some(ch) = decode_one_entity(&tail[1..semi]) {
                out.push(ch);
                rest = &tail[semi + 1..];
                continue;
            }
        }
        // Not a recognized entity — keep the '&' literally and move on.
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// Decode one entity body (the text between `&` and `;`) to a single character,
/// or `None` if unrecognized. Numeric references map to exactly one code point.
/// A numeric reference to a code point that XML 1.0 forbids (NUL, the C0/C1
/// control range except tab/newline/return, and the noncharacters U+FFFE/U+FFFF)
/// is rejected, so `&#0;` cannot inject a NUL or control character into a
/// scraped metadata value.
fn decode_one_entity(body: &str) -> Option<char> {
    match body {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let num = body.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            let ch = char::from_u32(code)?;
            is_xml_char(ch).then_some(ch)
        }
    }
}

/// Whether a character is permitted by XML 1.0's `Char` production — excludes
/// NUL and the C0/C1 control codes (bar tab, line feed, carriage return) and the
/// noncharacters U+FFFE/U+FFFF. Used to reject a numeric character reference to a
/// disallowed code point rather than emit it into a metadata string.
fn is_xml_char(ch: char) -> bool {
    matches!(ch,
        '\u{09}' | '\u{0A}' | '\u{0D}'
        | '\u{20}'..='\u{7E}'
        | '\u{85}'
        | '\u{00A0}'..='\u{D7FF}'
        | '\u{E000}'..='\u{FFFD}'
        | '\u{10000}'..='\u{10FFFF}'
    )
}

/// Truncate a string to [`MAX_FIELD_LEN`] bytes on a `char` boundary.
fn cap_len(s: &str) -> &str {
    if s.len() <= MAX_FIELD_LEN {
        return s;
    }
    let mut end = MAX_FIELD_LEN;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DC_RDF: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
      xmlns:dc="http://purl.org/dc/elements/1.1/"
      xmlns:xmp="http://ns.adobe.com/xap/1.0/"
      xmlns:pdf="http://ns.adobe.com/pdf/1.3/"
      pdf:Producer="Acrobat 7.0">
   <dc:title><rdf:Alt><rdf:li xml:lang="x-default">Annual &amp; Report</rdf:li></rdf:Alt></dc:title>
   <dc:creator><rdf:Seq><rdf:li>Jane Doe</rdf:li><rdf:li>John Roe</rdf:li></rdf:Seq></dc:creator>
   <dc:subject><rdf:Bag><rdf:li>finance</rdf:li><rdf:li>q4</rdf:li></rdf:Bag></dc:subject>
   <xmp:CreatorTool>LibreOffice</xmp:CreatorTool>
   <xmp:CreateDate>2024-01-01T12:00:00Z</xmp:CreateDate>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;

    #[test]
    fn scrapes_standard_packet() {
        let m = scrape(DC_RDF);
        assert_eq!(m.title.as_deref(), Some("Annual & Report")); // entity decoded
        assert_eq!(m.creators, vec!["Jane Doe", "John Roe"]);
        assert_eq!(m.subjects, vec!["finance", "q4"]);
        assert_eq!(m.creator_tool.as_deref(), Some("LibreOffice"));
        assert_eq!(m.create_date.as_deref(), Some("2024-01-01T12:00:00Z"));
        // pdf:Producer is given in the RDF attribute shorthand.
        assert_eq!(m.producer.as_deref(), Some("Acrobat 7.0"));
        assert!(!m.is_empty());
    }

    #[test]
    fn x_default_language_preferred() {
        let xml = r#"<dc:title><rdf:Alt>
            <rdf:li xml:lang="fr">Bonjour</rdf:li>
            <rdf:li xml:lang="x-default">Hello</rdf:li>
        </rdf:Alt></dc:title>"#;
        assert_eq!(alt_property(xml, "dc:title").as_deref(), Some("Hello"));
    }

    #[test]
    fn first_li_when_no_x_default() {
        let xml =
            r#"<dc:title><rdf:Alt><rdf:li xml:lang="fr">Bonjour</rdf:li></rdf:Alt></dc:title>"#;
        assert_eq!(alt_property(xml, "dc:title").as_deref(), Some("Bonjour"));
    }

    #[test]
    fn simple_element_property() {
        let xml = "<pdf:Producer>A &amp; B &lt;v2&gt;</pdf:Producer>";
        assert_eq!(
            simple_property(xml, "pdf:Producer").as_deref(),
            Some("A & B <v2>")
        );
    }

    #[test]
    fn numeric_character_reference_decoded() {
        // &#169; = '©', &#x2122; = '™'.
        let xml = "<pdf:Producer>Acme &#169; &#x2122;</pdf:Producer>";
        assert_eq!(
            simple_property(xml, "pdf:Producer").as_deref(),
            Some("Acme © ™")
        );
    }

    #[test]
    fn open_tag_requires_delimiter() {
        // <dc:titlebar> must not satisfy a search for <dc:title>.
        let xml = "<dc:titlebar>nope</dc:titlebar><dc:title>yes</dc:title>";
        assert_eq!(simple_property(xml, "dc:title").as_deref(), Some("yes"));
    }

    #[test]
    fn unknown_entity_is_not_expanded() {
        // A DTD-defined entity reference must be left verbatim — never resolved —
        // so an entity-expansion bomb cannot inflate the output. The scrape just
        // returns the literal text and terminates.
        let xml = "<pdf:Producer>&lol9; tail</pdf:Producer>";
        let v = simple_property(xml, "pdf:Producer").expect("value");
        assert_eq!(v, "&lol9; tail");
    }

    #[test]
    fn long_value_is_length_capped() {
        let big = "x".repeat(MAX_FIELD_LEN * 4);
        let xml = format!("<pdf:Producer>{big}</pdf:Producer>");
        let v = simple_property(&xml, "pdf:Producer").expect("value");
        assert!(v.len() <= MAX_FIELD_LEN, "value must be capped");
    }

    #[test]
    fn missing_property_is_none() {
        assert!(simple_property(DC_RDF, "pdf:Keywords").is_none());
        assert!(scrape("<x>no xmp here</x>").is_empty());
    }

    #[test]
    fn numeric_ref_to_control_char_is_rejected() {
        // &#0; (NUL) and &#x1; (a C0 control) must not be injected into the value;
        // the entity is left verbatim. A valid &#169; still decodes.
        let xml = "<pdf:Producer>a&#0;b&#x1;c &#169;</pdf:Producer>";
        let v = simple_property(xml, "pdf:Producer").expect("value");
        assert!(!v.contains('\u{0}'), "NUL must not be injected");
        assert!(!v.contains('\u{1}'), "control char must not be injected");
        assert!(v.contains('\u{A9}'), "valid char ref still decodes");
    }

    #[test]
    fn commented_out_property_is_ignored() {
        // A property commented out before the real one must not be matched.
        let xml = "<rdf:Description>\
            <!-- <pdf:Producer>FAKE</pdf:Producer> -->\
            <pdf:Producer>REAL</pdf:Producer></rdf:Description>";
        assert_eq!(scrape(xml).producer.as_deref(), Some("REAL"));
    }

    #[test]
    fn empty_container_does_not_leak_markup() {
        // An rdf:Alt/Seq/Bag with no rdf:li must yield no value — not the raw
        // markup of the (empty or foreign) container.
        assert!(alt_property("<dc:title><rdf:Alt></rdf:Alt></dc:title>", "dc:title").is_none());
        assert!(array_property("<dc:creator><rdf:Seq/></dc:creator>", "dc:creator").is_empty());
        // A genuinely simple text value still resolves.
        assert_eq!(
            alt_property("<dc:title>Plain</dc:title>", "dc:title").as_deref(),
            Some("Plain")
        );
    }

    #[test]
    fn multibyte_near_entity_window_does_not_panic() {
        // An '&' (with no ';') followed by a multibyte char straddling the
        // 12-byte entity-lookahead window must not panic the slice. Here '€'
        // occupies bytes 11..14 of `tail`, so a naive `tail[..12]` would split it.
        let xml = "<pdf:Producer>&xxxxxxxxxx\u{20AC}more</pdf:Producer>";
        let v = simple_property(xml, "pdf:Producer").expect("value");
        assert!(v.contains("more")); // decoded without panic; '&' kept literally
    }

    #[test]
    fn attribute_value_with_multibyte_is_decoded() {
        // A multibyte char inside a quoted attribute value must be returned
        // intact (no panic at the rest[q.len_utf8()..] / body.find(q) boundaries).
        let xml = "<rdf:Description pdf:Producer=\"\u{20AC}x\">";
        assert_eq!(
            attribute_value(xml, "pdf:Producer").as_deref(),
            Some("\u{20AC}x")
        );
    }

    #[test]
    fn utf16be_bom_decodes() {
        // "<pdf:Producer>Hi</pdf:Producer>" as UTF-16BE with a BOM.
        let s = "<pdf:Producer>Hi</pdf:Producer>";
        let mut bytes = vec![0xFE, 0xFF];
        for u in s.encode_utf16() {
            bytes.extend_from_slice(&u.to_be_bytes());
        }
        let xml = decode_text(&bytes);
        assert_eq!(simple_property(&xml, "pdf:Producer").as_deref(), Some("Hi"));
    }
}
