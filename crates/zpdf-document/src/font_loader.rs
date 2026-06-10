use zpdf_core::{ObjectId, PdfObject, Result};
use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};
use zpdf_parser::PdfFile;

use crate::page::PdfPage;

/// Load all fonts referenced by a page into a FontCache.
pub fn load_page_fonts(file: &PdfFile, page: &PdfPage) -> FontCache {
    let mut cache = FontCache::new();

    for (name, &font_ref) in &page.resources.fonts {
        match load_single_font(file, font_ref) {
            Ok(font) => {
                cache.insert(name.clone(), font);
            }
            Err(e) => {
                tracing::debug!("font {name} ({font_ref}): fallback - {e}");
                cache.insert(name.clone(), LoadedFont::new_placeholder(name.clone()));
            }
        }
    }

    cache
}

pub fn load_single_font(file: &PdfFile, font_ref: ObjectId) -> Result<LoadedFont> {
    let obj = file.resolve(font_ref)?;
    let dict = obj.as_dict()?;

    let subtype = dict.get_name("Subtype").unwrap_or("");
    let base_font = dict.get_name("BaseFont").unwrap_or("Unknown").to_string();

    let mut font = match subtype {
        "Type0" => load_type0_font(file, dict, base_font)?,
        "TrueType" => load_truetype_font(file, dict, base_font)?,
        "Type3" => load_type3_font(file, dict, base_font)?,
        "Type1" | "MMType1" => load_type1_font(file, dict, base_font)?,
        _ => LoadedFont::new_placeholder(base_font),
    };

    attach_text_mappings(file, dict, subtype, &mut font);
    Ok(font)
}

/// Attach the simple-font /Encoding, the symbolic flag, and /ToUnicode (for
/// text extraction) to a freshly-loaded font.
fn attach_text_mappings(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    subtype: &str,
    font: &mut LoadedFont,
) {
    // /ToUnicode lives at the top-level font dict for both simple and Type0 fonts.
    if let Ok(tu_ref) = dict.get_ref("ToUnicode") {
        if let Ok(data) = file.resolve_stream_data(tu_ref) {
            let map = zpdf_font::cmap::ToUnicodeMap::parse(&data);
            if !map.is_empty() {
                font.to_unicode = Some(map);
            }
        }
    }

    // /Encoding and the symbolic flag apply only to simple (non-composite) fonts.
    if subtype == "Type0" {
        return;
    }

    font.symbolic = font_descriptor_symbolic(file, dict);

    let encoding = if dict.get("Encoding").is_none() {
        // No explicit /Encoding: the Symbol/ZapfDingbats standard fonts carry their
        // own built-in encoding; other symbolic fonts use the font program's cmap.
        builtin_symbol_encoding(&font.base_font)
            .or_else(|| parse_encoding(file, dict, subtype, font.symbolic))
    } else {
        parse_encoding(file, dict, subtype, font.symbolic)
    };
    if let Some(enc) = encoding {
        font.encoding = Some(enc);
    }

    // With encoding and widths in place, recover Quartz-subset glyphs that are
    // reachable through no declared encoding (charset entries named ".notdef").
    font.map_unencoded_orphans();
}

/// The built-in encoding for the Symbol / ZapfDingbats standard fonts, matched by
/// BaseFont (ignoring any subset prefix). Used when no explicit /Encoding is given,
/// so symbolic Symbol/Dingbats text is still extractable via the glyph list.
fn builtin_symbol_encoding(base_font: &str) -> Option<zpdf_font::encoding::Encoding> {
    use zpdf_font::encoding::{base_encoding_by_name, Encoding};
    let name = base_font.rsplit('+').next().unwrap_or(base_font);
    let canonical = if name.contains("ZapfDingbats") || name.contains("Dingbats") {
        "ZapfDingbats"
    } else if name.contains("Symbol") {
        "Symbol"
    } else {
        return None;
    };
    base_encoding_by_name(canonical).map(Encoding::from_base)
}

/// Read the FontDescriptor /Flags and decide whether the font is symbolic
/// (bit 3 set, bit 6 clear).
fn font_descriptor_symbolic(file: &PdfFile, dict: &zpdf_core::PdfDict) -> bool {
    let fd_ref = match dict.get_ref("FontDescriptor") {
        Ok(r) => r,
        Err(_) => return false,
    };
    let flags = file
        .resolve(fd_ref)
        .ok()
        .and_then(|o| o.as_dict().ok().and_then(|d| d.get_i64("Flags").ok()));
    matches!(flags, Some(f) if (f & 4) != 0 && (f & 32) == 0)
}

/// Build the effective simple-font encoding from /Encoding (a name, a dict with
/// /BaseEncoding + /Differences, or absent).
fn parse_encoding(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    subtype: &str,
    symbolic: bool,
) -> Option<zpdf_font::encoding::Encoding> {
    use zpdf_font::encoding::{base_encoding_by_name, Encoding};

    let enc_obj = match dict.get("Encoding").cloned() {
        Some(PdfObject::Ref(r)) => file.resolve(r).ok(),
        other => other,
    };

    match enc_obj {
        Some(PdfObject::Name(n)) => base_encoding_by_name(n.as_str()).map(Encoding::from_base),
        Some(PdfObject::Dict(enc_dict)) => {
            let base = enc_dict
                .get_name("BaseEncoding")
                .ok()
                .and_then(base_encoding_by_name)
                .unwrap_or_else(|| default_simple_base(subtype));
            let mut encoding = Encoding::from_base(base);
            apply_differences(&enc_dict, &mut encoding);
            Some(encoding)
        }
        // No /Encoding: symbolic fonts use their built-in cmap; others get a default.
        _ if symbolic => None,
        _ => Some(Encoding::from_base(default_simple_base(subtype))),
    }
}

fn default_simple_base(subtype: &str) -> &'static zpdf_font::encoding::EncodingTable {
    match subtype {
        "TrueType" => &zpdf_font::encoding::WIN_ANSI_ENCODING,
        _ => &zpdf_font::encoding::STANDARD_ENCODING,
    }
}

fn apply_differences(enc_dict: &zpdf_core::PdfDict, encoding: &mut zpdf_font::encoding::Encoding) {
    if let Ok(diffs) = enc_dict.get_array("Differences") {
        let mut code = 0u32;
        for obj in diffs {
            match obj {
                PdfObject::Integer(n) => code = (*n).max(0) as u32,
                PdfObject::Name(name) => {
                    if code <= 255 {
                        encoding.apply_difference(code as u8, name.as_str());
                    }
                    code += 1;
                }
                _ => {}
            }
        }
    }
}

fn load_type0_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
) -> Result<LoadedFont> {
    let descendants = dict.get_array("DescendantFonts")?;
    let desc_ref = descendants
        .first()
        .ok_or_else(|| zpdf_core::Error::MissingKey("DescendantFonts[0]".into()))?
        .as_ref()?;

    let desc_obj = file.resolve(desc_ref)?;
    let desc_dict = desc_obj.as_dict()?;

    let cid_widths = parse_cid_widths(file, desc_dict);

    let font_data = extract_font_file(file, desc_dict);

    match font_data {
        Some(data) => {
            let mut font = LoadedFont::new_with_data(
                PdfFontType::Type0CidType2,
                base_font,
                data,
                cid_widths,
            );
            // /CIDToGIDMap stream: explicit CID → GID table, authoritative for
            // CIDFontType2 (TrueType-based) descendants. A raw-CFF CIDFontType0
            // descendant keeps its charset-derived map built in new_with_data —
            // there /CIDToGIDMap is not even a legal key.
            if let Some(map) = parse_cid_to_gid_stream(file, desc_dict) {
                let subtype = desc_dict.get_name("Subtype").unwrap_or("");
                if subtype == "CIDFontType2" || font.cid_to_gid.is_none() {
                    font.cid_to_gid = Some(map);
                }
            }
            Ok(font)
        }
        None => Ok(LoadedFont::new_placeholder(base_font)),
    }
}

/// Decode a /CIDToGIDMap stream into a CID → GID table: two bytes per CID,
/// big-endian, indexed by CID. Returns `None` for /Identity, absence, or any
/// non-stream form, which keeps the identity (or charset-derived) behavior.
/// CIDs mapped to GID 0 (.notdef) are omitted — `glyph_outline` treats a
/// missing entry as "no glyph", which matches the spec semantics.
fn parse_cid_to_gid_stream(
    file: &PdfFile,
    desc_dict: &zpdf_core::PdfDict,
) -> Option<std::collections::HashMap<u16, u16>> {
    let stream_ref = match desc_dict.get("CIDToGIDMap") {
        Some(PdfObject::Ref(r)) => *r,
        // /Identity (the common name form), absent, or malformed.
        _ => return None,
    };
    let data = match file.resolve_stream_data(stream_ref) {
        Ok(d) => d,
        Err(e) => {
            // e.g. an indirect /Identity name, or an undecodable stream.
            tracing::debug!("CIDToGIDMap {stream_ref}: not a decodable stream - {e}");
            return None;
        }
    };
    let mut map = std::collections::HashMap::new();
    for (cid, gid_bytes) in data
        .chunks_exact(2)
        .enumerate()
        .take(u16::MAX as usize + 1)
    {
        let gid = u16::from_be_bytes([gid_bytes[0], gid_bytes[1]]);
        if gid != 0 {
            map.insert(cid as u16, gid);
        }
    }
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

fn load_truetype_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
) -> Result<LoadedFont> {
    let cid_widths = parse_simple_widths(file, dict);
    let font_data = extract_font_file_from_descriptor(file, dict);

    match font_data {
        Some(data) => Ok(LoadedFont::new_with_data(
            PdfFontType::TrueType,
            base_font,
            data,
            cid_widths,
        )),
        None => Ok(LoadedFont::new_standard(base_font.clone())
            .unwrap_or_else(|| LoadedFont::new_placeholder(base_font))),
    }
}

fn load_type3_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
) -> Result<LoadedFont> {
    use std::sync::Arc;

    // All four Type3 keys are commonly emitted as indirect objects; a direct-only
    // read would silently drop every glyph, so resolve one level of indirection.

    // FontMatrix: typically [0.001 0 0 -0.001 0 0] for 1000-unit glyph space
    let font_matrix = {
        let mut m = [0.001, 0.0, 0.0, -0.001, 0.0, 0.0];
        if let Some(arr) = resolve_array(file, dict, "FontMatrix") {
            for (i, obj) in arr.iter().enumerate().take(6) {
                if let Ok(v) = obj.as_f64() {
                    m[i] = v;
                }
            }
        }
        m
    };

    // Encoding/Differences → glyph name list
    let mut encoding = Vec::new();
    if let Some(enc_dict) = resolve_dict(file, dict, "Encoding") {
        if let Some(diffs) = resolve_array(file, &enc_dict, "Differences") {
            let mut current_code = 0usize;
            for obj in &diffs {
                match obj {
                    PdfObject::Integer(n) => {
                        current_code = *n as usize;
                        while encoding.len() < current_code {
                            encoding.push(String::new());
                        }
                    }
                    PdfObject::Name(n) => {
                        while encoding.len() <= current_code {
                            encoding.push(String::new());
                        }
                        encoding[current_code] = n.0.clone();
                        current_code += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    // CharProcs: name → stream ref
    let mut char_procs = std::collections::HashMap::new();
    if let Some(cp_dict) = resolve_dict(file, dict, "CharProcs") {
        for (name, obj) in &cp_dict.0 {
            if let PdfObject::Ref(r) = obj {
                if let Ok(data) = file.resolve_stream_data(*r) {
                    char_procs.insert(name.0.clone(), Arc::from(data));
                }
            }
        }
    }

    // Widths
    let first_char = dict.get_i64("FirstChar").unwrap_or(0) as u16;
    let widths: Vec<f64> = resolve_array(file, dict, "Widths")
        .unwrap_or_default()
        .iter()
        .map(|o| o.as_f64().unwrap_or(0.0))
        .collect();

    let font = LoadedFont {
        font_type: zpdf_font::PdfFontType::Type3 {
            font_matrix,
            char_procs,
            encoding,
            widths,
            first_char,
        },
        base_font,
        font_data: None,
        cid_widths: CidWidths::new(1000.0),
        units_per_em: 1000.0,
        ascent: 880.0,
        descent: -120.0,
        cid_to_gid: None,
        builtin_encoding_gids: None,
        orphan_gids: Vec::new(),
        encoding: None,
        to_unicode: None,
        symbolic: false,
        type1: None,
    };

    Ok(font)
}

fn load_type1_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
) -> Result<LoadedFont> {
    let cid_widths = parse_simple_widths(file, dict);
    let font_data = extract_font_file_from_descriptor(file, dict);

    match font_data {
        Some(data) => Ok(LoadedFont::new_with_data(
            PdfFontType::Type1,
            base_font,
            data,
            cid_widths,
        )),
        None => Ok(LoadedFont::new_standard(base_font.clone())
            .unwrap_or_else(|| LoadedFont::new_placeholder(base_font))),
    }
}

/// Extract embedded font binary from FontDescriptor → FontFile2 (TrueType).
fn extract_font_file(file: &PdfFile, cid_dict: &zpdf_core::PdfDict) -> Option<Vec<u8>> {
    let fd_ref = cid_dict.get_ref("FontDescriptor").ok()?;
    let fd_obj = file.resolve(fd_ref).ok()?;
    let fd_dict = fd_obj.as_dict().ok()?;

    // Try FontFile2 (TrueType), then FontFile3 (OpenType/CFF), then FontFile (Type1)
    for key in &["FontFile2", "FontFile3", "FontFile"] {
        if let Ok(ff_ref) = fd_dict.get_ref(key) {
            if let Ok(data) = file.resolve_stream_data(ff_ref) {
                if !data.is_empty() {
                    return Some(data);
                }
            }
        }
    }
    None
}

fn extract_font_file_from_descriptor(
    file: &PdfFile,
    font_dict: &zpdf_core::PdfDict,
) -> Option<Vec<u8>> {
    let fd_ref = font_dict.get_ref("FontDescriptor").ok()?;
    let fd_obj = file.resolve(fd_ref).ok()?;
    let fd_dict = fd_obj.as_dict().ok()?;

    for key in &["FontFile2", "FontFile3", "FontFile"] {
        if let Ok(ff_ref) = fd_dict.get_ref(key) {
            if let Ok(data) = file.resolve_stream_data(ff_ref) {
                if !data.is_empty() {
                    return Some(data);
                }
            }
        }
    }
    None
}

/// Fetch an array value, resolving one level of indirect reference. pdftex (and
/// many other producers) commonly emit `/Widths` and `/W` as indirect objects,
/// which a plain `get_array` would miss (leaving every glyph at the default width).
fn resolve_array(file: &PdfFile, dict: &zpdf_core::PdfDict, key: &str) -> Option<Vec<PdfObject>> {
    match dict.get(key) {
        Some(PdfObject::Array(a)) => Some(a.clone()),
        Some(PdfObject::Ref(id)) => file
            .resolve(*id)
            .ok()
            .and_then(|o| o.as_array().ok().map(|a| a.to_vec())),
        _ => None,
    }
}

/// Fetch a dictionary value, resolving one level of indirect reference, in the
/// same spirit as [`resolve_array`] (Type3 producers commonly emit /CharProcs
/// and /Encoding as indirect objects).
fn resolve_dict(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    key: &str,
) -> Option<zpdf_core::PdfDict> {
    match dict.get(key) {
        Some(PdfObject::Dict(d)) => Some(d.clone()),
        Some(PdfObject::Ref(id)) => file
            .resolve(*id)
            .ok()
            .and_then(|o| o.as_dict().ok().cloned()),
        _ => None,
    }
}

/// Parse CID /W array: format is [cid [w1 w2 ...]] or [cid_first cid_last w]
fn parse_cid_widths(file: &PdfFile, dict: &zpdf_core::PdfDict) -> CidWidths {
    let dw = dict.get_f64("DW").unwrap_or(1000.0);
    let mut widths = CidWidths::new(dw);

    let w_array = match resolve_array(file, dict, "W") {
        Some(arr) => arr,
        None => return widths,
    };

    let mut i = 0;
    while i < w_array.len() {
        let cid_start = match w_array[i].as_i64() {
            Ok(v) => v as u16,
            Err(_) => break,
        };
        i += 1;
        if i >= w_array.len() {
            break;
        }

        match &w_array[i] {
            PdfObject::Array(arr) => {
                // [cid_start [w1 w2 w3 ...]]
                for (j, obj) in arr.iter().enumerate() {
                    let Some(cid) = cid_start.checked_add(j as u16) else {
                        break;
                    };
                    if let Ok(w) = obj.as_f64() {
                        widths.set(cid, w);
                    }
                }
                i += 1;
            }
            PdfObject::Integer(_) | PdfObject::Real(_) => {
                // [cid_start cid_end width]
                let cid_end = w_array[i].as_i64().unwrap_or(cid_start as i64) as u16;
                i += 1;
                if i < w_array.len() {
                    let w = w_array[i].as_f64().unwrap_or(dw);
                    for cid in cid_start..=cid_end {
                        widths.set(cid, w);
                    }
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    widths
}

fn parse_simple_widths(file: &PdfFile, dict: &zpdf_core::PdfDict) -> CidWidths {
    let first_char = dict.get_i64("FirstChar").unwrap_or(0) as u16;
    let mut widths = CidWidths::new(1000.0);

    if let Some(arr) = resolve_array(file, dict, "Widths") {
        for (j, obj) in arr.iter().enumerate() {
            let Some(code) = first_char.checked_add(j as u16) else {
                break;
            };
            if let Ok(w) = obj.as_f64() {
                widths.set(code, w);
            }
        }
    }

    widths
}
