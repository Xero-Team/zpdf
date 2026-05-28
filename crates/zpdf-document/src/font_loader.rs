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

fn load_single_font(file: &PdfFile, font_ref: ObjectId) -> Result<LoadedFont> {
    let obj = file.resolve(font_ref)?;
    let dict = obj.as_dict()?;

    let subtype = dict.get_name("Subtype").unwrap_or("");
    let base_font = dict.get_name("BaseFont").unwrap_or("Unknown").to_string();

    match subtype {
        "Type0" => load_type0_font(file, dict, base_font),
        "TrueType" => load_truetype_font(file, dict, base_font),
        "Type3" => load_type3_font(file, dict, base_font),
        "Type1" => load_type1_font(file, dict, base_font),
        _ => Ok(LoadedFont::new_placeholder(base_font)),
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

    let cid_widths = parse_cid_widths(desc_dict);

    let font_data = extract_font_file(file, desc_dict);

    match font_data {
        Some(data) => Ok(LoadedFont::new_with_data(
            PdfFontType::Type0CidType2,
            base_font,
            data,
            cid_widths,
        )),
        None => Ok(LoadedFont::new_placeholder(base_font)),
    }
}

fn load_truetype_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
) -> Result<LoadedFont> {
    let cid_widths = parse_simple_widths(dict);
    let font_data = extract_font_file_from_descriptor(file, dict);

    match font_data {
        Some(data) => Ok(LoadedFont::new_with_data(
            PdfFontType::TrueType,
            base_font,
            data,
            cid_widths,
        )),
        None => Ok(LoadedFont::new_placeholder(base_font)),
    }
}

fn load_type3_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
) -> Result<LoadedFont> {
    use std::sync::Arc;

    // FontMatrix: typically [0.001 0 0 -0.001 0 0] for 1000-unit glyph space
    let font_matrix = if let Ok(arr) = dict.get_array("FontMatrix") {
        let mut m = [0.001, 0.0, 0.0, -0.001, 0.0, 0.0];
        for (i, obj) in arr.iter().enumerate().take(6) {
            if let Ok(v) = obj.as_f64() {
                m[i] = v;
            }
        }
        m
    } else {
        [0.001, 0.0, 0.0, -0.001, 0.0, 0.0]
    };

    // Encoding/Differences → glyph name list
    let mut encoding = Vec::new();
    if let Some(PdfObject::Dict(enc_dict)) = dict.get("Encoding") {
        if let Ok(diffs) = enc_dict.get_array("Differences") {
            let mut current_code = 0usize;
            for obj in diffs {
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
    if let Ok(cp_dict) = dict.get_dict("CharProcs") {
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
    let widths: Vec<f64> = dict
        .get_array("Widths")
        .unwrap_or(&[])
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
    };

    Ok(font)
}

fn load_type1_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
) -> Result<LoadedFont> {
    let cid_widths = parse_simple_widths(dict);
    let font_data = extract_font_file_from_descriptor(file, dict);

    match font_data {
        Some(data) => Ok(LoadedFont::new_with_data(
            PdfFontType::Type1,
            base_font,
            data,
            cid_widths,
        )),
        None => Ok(LoadedFont::new_placeholder(base_font)),
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

/// Parse CID /W array: format is [cid [w1 w2 ...]] or [cid_first cid_last w]
fn parse_cid_widths(dict: &zpdf_core::PdfDict) -> CidWidths {
    let dw = dict.get_f64("DW").unwrap_or(1000.0);
    let mut widths = CidWidths::new(dw);

    let w_array = match dict.get_array("W") {
        Ok(arr) => arr.to_vec(),
        Err(_) => return widths,
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
                    if let Ok(w) = obj.as_f64() {
                        widths.set(cid_start + j as u16, w);
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

fn parse_simple_widths(dict: &zpdf_core::PdfDict) -> CidWidths {
    let first_char = dict.get_i64("FirstChar").unwrap_or(0) as u16;
    let mut widths = CidWidths::new(1000.0);

    if let Ok(arr) = dict.get_array("Widths") {
        for (j, obj) in arr.iter().enumerate() {
            if let Ok(w) = obj.as_f64() {
                widths.set(first_char + j as u16, w);
            }
        }
    }

    widths
}
