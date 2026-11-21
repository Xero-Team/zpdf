use zpdf_core::{ObjectId, PdfObject, Result};
use zpdf_font::{CidWidths, FontCache, FontProvider, LoadedFont, PdfFontType};
use zpdf_parser::PdfFile;

use crate::page::PdfPage;
use crate::SharedFonts;

/// Load all fonts referenced by a page into a FontCache, reusing what the document
/// has already parsed. Called through [`PdfDocument::load_page_fonts`], which owns the
/// shared cache.
pub(crate) fn load_page_fonts(
    file: &PdfFile,
    page: &PdfPage,
    shared: &mut SharedFonts,
    provider: Option<&std::sync::Arc<FontProvider>>,
) -> FontCache {
    // Most pages reference only a few fonts. Reserve for their actual resources
    // rather than allocating three 256-entry maps on every warm page load.
    // Nested Forms and drawing fallbacks still grow the maps without evicting IDs.
    let mut cache = FontCache::with_preallocated_capacity(page.resources.fonts.len().min(256));
    shared.refresh_provider(provider.map(|provider| provider.generation()));
    if let Some(provider) = provider {
        cache = cache.with_provider(std::sync::Arc::clone(provider));
    }
    let max_bytes = file.limits().max_font_cache_bytes;

    for (name, &font_ref) in &page.resources.fonts {
        match shared_or_load(file, font_ref, shared, provider) {
            Ok((font, diagnostics)) => {
                if cache
                    .admit_with_diagnostics(
                        name.clone(),
                        font,
                        diagnostics,
                        Some(font_ref),
                        max_bytes,
                    )
                    .is_err()
                {
                    tracing::warn!(
                        "font cache byte limit ({max_bytes}) or ID capacity reached; could not admit {name}"
                    );
                }
            }
            Err(e) => {
                tracing::debug!("font {name} ({font_ref}): fallback - {e}");
                let _ = insert_placeholder(&mut cache, name, max_bytes);
            }
        }
    }

    cache
}

/// Load one font dictionary through the document's shared cache. Called through
/// [`PdfDocument::shared_font`](crate::PdfDocument::shared_font), for fonts
/// that Form XObjects and annotation appearances reference outside the page's
/// own `/Resources`.
pub(crate) fn load_shared_font(
    file: &PdfFile,
    font_ref: ObjectId,
    shared: &mut SharedFonts,
    provider: Option<&std::sync::Arc<FontProvider>>,
) -> Result<(std::sync::Arc<LoadedFont>, zpdf_core::ProcessingDiagnostics)> {
    shared.refresh_provider(provider.map(|provider| provider.generation()));
    shared_or_load(file, font_ref, shared, provider)
}

/// The font parsed for `font_ref` by an earlier page or form of this document,
/// or a fresh parse of it. `LoadedFont` is immutable once built, so every cache
/// can share one without copying it. A fresh parse is retained for the rest of
/// the document when the shared budget allows; past that, later loads parse it
/// again rather than growing without bound.
fn shared_or_load(
    file: &PdfFile,
    font_ref: ObjectId,
    shared: &mut SharedFonts,
    provider: Option<&std::sync::Arc<FontProvider>>,
) -> Result<(std::sync::Arc<LoadedFont>, zpdf_core::ProcessingDiagnostics)> {
    if let Some(font) = shared.get(font_ref) {
        return Ok(font);
    }
    let (font, diagnostics) =
        load_single_font_with_diagnostics(file, font_ref, provider.map(|p| p.as_ref()))?;
    Ok(shared.admit(
        font_ref,
        font,
        diagnostics,
        file.limits().max_font_cache_bytes,
    ))
}

/// Cache a placeholder under `name`, warning when even that does not fit.
fn insert_placeholder(
    cache: &mut FontCache,
    name: &str,
    max_bytes: u64,
) -> Option<zpdf_font::FontId> {
    let inserted = cache.try_insert_with_limit(
        name.to_string(),
        LoadedFont::new_placeholder(name.to_string()),
        max_bytes,
    );
    if inserted.is_none() {
        tracing::warn!(
            "font cache byte limit ({max_bytes}) or ID capacity reached; using placeholder for {name}"
        );
    }
    inserted
}

pub fn load_single_font(file: &PdfFile, font_ref: ObjectId) -> Result<LoadedFont> {
    load_single_font_with_provider(file, font_ref, None)
}

pub fn load_single_font_with_provider(
    file: &PdfFile,
    font_ref: ObjectId,
    provider: Option<&FontProvider>,
) -> Result<LoadedFont> {
    Ok(load_single_font_with_diagnostics(file, font_ref, provider)?.0)
}

/// Like [`load_single_font_with_provider`], also reporting diagnostics when an
/// embedded font program fails to decode or parse and falls back.
pub fn load_single_font_with_diagnostics(
    file: &PdfFile,
    font_ref: ObjectId,
    provider: Option<&FontProvider>,
) -> Result<(LoadedFont, zpdf_core::ProcessingDiagnostics)> {
    let obj = file.resolve(font_ref)?;
    let dict = obj.as_dict()?;
    let (font, mut diagnostics) = load_single_font_dict_with_diagnostics(file, dict, provider)?;
    if !diagnostics.is_empty() {
        diagnostics.first_failed_object.get_or_insert(font_ref);
    }
    Ok((font, diagnostics))
}

/// Load a font from its (already-resolved) font dictionary. Used both by
/// [`load_single_font`] and for inline font dicts in form resources (e.g. a
/// synthesized field appearance referencing a standard Helvetica).
pub fn load_single_font_dict(file: &PdfFile, dict: &zpdf_core::PdfDict) -> Result<LoadedFont> {
    load_single_font_dict_with_provider(file, dict, None)
}

pub fn load_single_font_dict_with_provider(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    provider: Option<&FontProvider>,
) -> Result<LoadedFont> {
    Ok(load_single_font_dict_with_diagnostics(file, dict, provider)?.0)
}

/// Like [`load_single_font_dict_with_provider`], also reporting diagnostics
/// when an embedded font program fails to decode or parse and falls back.
pub fn load_single_font_dict_with_diagnostics(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    provider: Option<&FontProvider>,
) -> Result<(LoadedFont, zpdf_core::ProcessingDiagnostics)> {
    let subtype = dict.get_name("Subtype").unwrap_or("");
    let base_font = dict.get_name("BaseFont").unwrap_or("Unknown").to_string();

    let to_unicode = parse_to_unicode(file, dict);
    let (mut font, diagnostics) = match subtype {
        "Type0" => load_type0_font(file, dict, base_font, provider, to_unicode.clone())?,
        "TrueType" => load_simple_font(file, dict, base_font, PdfFontType::TrueType, provider)?,
        "Type3" => (
            load_type3_font(file, dict, base_font)?,
            zpdf_core::ProcessingDiagnostics::default(),
        ),
        "Type1" | "MMType1" => {
            load_simple_font(file, dict, base_font, PdfFontType::Type1, provider)?
        }
        _ => (
            LoadedFont::new_placeholder(base_font),
            zpdf_core::ProcessingDiagnostics::default(),
        ),
    };

    attach_text_mappings(file, dict, subtype, to_unicode, &mut font);
    // A substituted composite font needs /ToUnicode (attached just above) to
    // route CIDs through the system face's Unicode cmap.
    if font.cid_to_gid.is_none() {
        font.build_substitute_cid_to_gid();
    }
    // FontDescriptor weight/width/slant → variation axes (variable fonts only).
    if subtype != "Type0" {
        apply_descriptor_variations(file, dict, &mut font);
    }
    Ok((font, diagnostics))
}

fn descriptor_hints(
    file: &PdfFile,
    descriptor: Option<&zpdf_core::PdfDict>,
) -> zpdf_font::system::SubstituteHints {
    let mut hints = zpdf_font::system::SubstituteHints::default();
    // /FontDescriptor must be indirect per spec, but AutoCAD and other producers
    // inline it; resolve_dict accepts both forms.
    if let Some(fd) = descriptor {
        if let Ok(flags) = fd.get_i64("Flags") {
            hints.fixed_pitch = flags & 1 != 0;
            hints.serif = flags & 2 != 0;
            hints.italic = flags & 64 != 0;
            hints.bold = flags & (1 << 18) != 0; // ForceBold
        }
        if let Some(w) = resolve_f64(file, fd, "StemV") {
            hints.bold |= w >= 160.0;
        }
        // ISO 32000-1 Table 122: /FontWeight 400 is normal, 700 is bold
        // (600+ covers SemiBold/Bold/ExtraBold/Black).
        if let Some(weight) = resolve_f64(file, fd, "FontWeight") {
            hints.bold |= weight >= 600.0;
        }
        if let Some(angle) = resolve_f64(file, fd, "ItalicAngle") {
            hints.italic |= angle != 0.0;
        }
    }
    hints
}

fn resolve_f64(file: &PdfFile, dict: &zpdf_core::PdfDict, key: &str) -> Option<f64> {
    match dict.get(key) {
        Some(PdfObject::Ref(id)) => file.resolve(*id).ok().and_then(|o| o.as_f64().ok()),
        Some(other) => other.as_f64().ok(),
        None => None,
    }
    .filter(|v| v.is_finite())
}

/// Prefer `/BaseFont` for substitution lookups; fall back to the descriptor's
/// `/FontName` when `/BaseFont` was omitted.
fn substitute_font_name<'a>(
    base_font: &'a str,
    descriptor: Option<&'a zpdf_core::PdfDict>,
) -> &'a str {
    if base_font != "Unknown" {
        return base_font;
    }
    descriptor
        .and_then(|fd| fd.get_name("FontName").ok())
        .unwrap_or(base_font)
}

/// Try to substitute an installed system font for a simple font whose embedded
/// program is absent or unusable. The PDF /Widths stay authoritative for
/// advances when present; otherwise the standard-14 metrics (if the name
/// matches one) seed the widths.
fn try_system_substitute_simple(
    file: &PdfFile,
    descriptor: Option<&zpdf_core::PdfDict>,
    base_font: &str,
    font_type: PdfFontType,
    mut cid_widths: CidWidths,
    provider: Option<&FontProvider>,
) -> Option<LoadedFont> {
    let hints = descriptor_hints(file, descriptor);
    let lookup_name = substitute_font_name(base_font, descriptor);
    let m = match provider {
        Some(provider) => provider.find_font(lookup_name, hints, None),
        None => zpdf_font::system::find_system_font(lookup_name, hints, None),
    }?;
    if cid_widths.is_empty() && !cid_widths.has_authoritative_default() {
        if let Some(metrics) = zpdf_font::standard_fonts::lookup(lookup_name) {
            for (code, &w) in metrics.widths.iter().enumerate() {
                if w > 0 {
                    cid_widths.set(code as u16, w as f64);
                }
            }
        }
    }
    LoadedFont::new_substitute(
        font_type,
        base_font.to_string(),
        m.data,
        m.face_index,
        cid_widths,
    )
}

/// Substitute path shared by non-embedded simple fonts and simple fonts whose
/// embedded program failed to decode or parse. Preserves PDF `/Widths` even
/// when no system or provider face is available.
fn fallback_simple_font(
    file: &PdfFile,
    descriptor: Option<&zpdf_core::PdfDict>,
    base_font: String,
    font_type: PdfFontType,
    cid_widths: CidWidths,
    provider: Option<&FontProvider>,
) -> LoadedFont {
    try_system_substitute_simple(
        file,
        descriptor,
        &base_font,
        font_type.clone(),
        cid_widths.clone(),
        provider,
    )
    .or_else(|| {
        let mut standard = LoadedFont::new_standard(base_font.clone())?;
        if !cid_widths.is_empty() {
            standard.cid_widths = cid_widths.clone();
        }
        Some(standard)
    })
    .unwrap_or_else(|| {
        let mut font = LoadedFont::new_placeholder(base_font);
        font.font_type = font_type;
        font.cid_widths = cid_widths;
        font
    })
}

fn embedded_fallback_diagnostics(
    object_id: Option<ObjectId>,
    error: Option<&zpdf_core::Error>,
) -> zpdf_core::ProcessingDiagnostics {
    let mut diagnostics = zpdf_core::ProcessingDiagnostics::default();
    diagnostics.record_embedded_font_fallback(object_id);
    // Resource limits on an embedded font stream must remain visible as
    // incomplete output even if a substitute face is available. Ordinary
    // stream-decode errors defer `decode_failures` to the interpreter so a
    // substitute that draws every visible glyph can still complete cleanly.
    if let Some(err) = error {
        if matches!(
            err,
            zpdf_core::Error::RecursionLimit(_)
                | zpdf_core::Error::StreamSizeLimit(_)
                | zpdf_core::Error::StringLengthLimit(_)
        ) {
            if let Some(id) = object_id {
                diagnostics.record_object_failure(id, err);
            } else {
                diagnostics.record_decode_failure(err);
            }
        }
    }
    diagnostics
}

fn parse_to_unicode(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
) -> Option<zpdf_font::cmap::ToUnicodeMap> {
    let tu_ref = dict.get_ref("ToUnicode").ok()?;
    let data = file.resolve_stream_data(tu_ref).ok()?;
    let map = zpdf_font::cmap::ToUnicodeMap::parse(&data);
    (!map.is_empty()).then_some(map)
}

/// Attach the simple-font /Encoding, the symbolic flag, and /ToUnicode (for
/// text extraction) to a freshly-loaded font.
fn attach_text_mappings(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    subtype: &str,
    to_unicode: Option<zpdf_font::cmap::ToUnicodeMap>,
    font: &mut LoadedFont,
) {
    // /ToUnicode lives at the top-level font dict for both simple and Type0 fonts.
    if font.to_unicode.is_none() {
        font.to_unicode = to_unicode;
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
    let flags = resolve_dict(file, dict, "FontDescriptor").and_then(|d| d.get_i64("Flags").ok());
    matches!(flags, Some(f) if (f & 4) != 0 && (f & 32) == 0)
}

/// Resolve the FontDescriptor dict carrying the embedded program's metadata,
/// handling the Type0 indirection (the descriptor lives on the descendant
/// CIDFont, not the top-level Type0 dict).
fn font_descriptor_dict(file: &PdfFile, dict: &zpdf_core::PdfDict) -> Option<zpdf_core::PdfDict> {
    let host = if dict.get_name("Subtype").unwrap_or("") == "Type0" {
        let descendants = resolve_array(file, dict, "DescendantFonts")?;
        descendant_cid_dict(file, &descendants)?
    } else {
        dict.clone()
    };
    resolve_dict(file, &host, "FontDescriptor")
}

/// The first /DescendantFonts entry as a dict. The spec requires an indirect
/// reference, but AutoCAD and other producers inline the CIDFont dict directly
/// in the array; accept both forms.
fn descendant_cid_dict(file: &PdfFile, descendants: &[PdfObject]) -> Option<zpdf_core::PdfDict> {
    match descendants.first()? {
        PdfObject::Dict(d) => Some(d.clone()),
        PdfObject::Ref(r) => file.resolve(*r).ok()?.as_dict().ok().cloned(),
        _ => None,
    }
}

/// Map a `/FontStretch` name to its OpenType `wdth`-axis percentage (Table 122).
fn font_stretch_pct(name: &str) -> Option<f64> {
    Some(match name {
        "UltraCondensed" => 50.0,
        "ExtraCondensed" => 62.5,
        "Condensed" => 75.0,
        "SemiCondensed" => 87.5,
        "Normal" => 100.0,
        "SemiExpanded" => 112.5,
        "Expanded" => 125.0,
        "ExtraExpanded" => 150.0,
        "UltraExpanded" => 200.0,
        _ => return None,
    })
}

/// Drive a variable font's OpenType axes from the FontDescriptor's selectors
/// (`/FontWeight`→`wght`, `/FontStretch`→`wdth`, `/ItalicAngle`→`slnt`, Italic
/// flag→`ital`). A no-op for static fonts (the axes simply do not exist), so it
/// is applied to every font; the common selector-less font is left untouched.
fn apply_descriptor_variations(file: &PdfFile, dict: &zpdf_core::PdfDict, font: &mut LoadedFont) {
    let Some(fd) = font_descriptor_dict(file, dict) else {
        return;
    };
    apply_variations_from_descriptor(&fd, font);
}

fn apply_variations_from_descriptor(fd: &zpdf_core::PdfDict, font: &mut LoadedFont) {
    let weight = fd.get_f64("FontWeight").ok();
    let width_pct = fd.get_name("FontStretch").ok().and_then(font_stretch_pct);
    let italic_angle = fd.get_f64("ItalicAngle").ok();
    let italic = fd.get_i64("Flags").map(|f| f & 64 != 0).unwrap_or(false);
    if weight.is_some() || width_pct.is_some() || italic_angle.is_some() || italic {
        font.set_variations(weight, width_pct, italic_angle, italic);
    }
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
        let mut code = Some(0u16);
        for obj in diffs {
            match obj {
                PdfObject::Integer(n) => code = u8::try_from(*n).ok().map(u16::from),
                PdfObject::Name(name) => {
                    if let Some(current) = code {
                        encoding.apply_difference(current as u8, name.as_str());
                        code = current.checked_add(1).filter(|&next| next <= 255);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Resolve a Type0 font's /Encoding into a code → CID CMap: a predefined
/// name, or an embedded CMap stream. Unknown legacy CMaps fall back to
/// Identity-H with a warning.
fn parse_type0_encoding(file: &PdfFile, dict: &zpdf_core::PdfDict) -> zpdf_font::cmap::CidCMap {
    use zpdf_font::cmap::CidCMap;
    // Unknown (legacy byte-encoded) CMaps degrade to Identity, but the
    // writing mode is still known from the -V suffix and kept.
    fn identity_fallback(name: &str) -> CidCMap {
        let wmode = name.ends_with("-V") as u8;
        tracing::warn!(
            "unsupported predefined CMap {name}; using Identity-{}",
            if wmode == 1 { "V" } else { "H" }
        );
        CidCMap::identity(wmode)
    }
    match dict.get("Encoding") {
        Some(PdfObject::Name(n)) => {
            CidCMap::predefined(n.as_str()).unwrap_or_else(|| identity_fallback(n.as_str()))
        }
        Some(PdfObject::Ref(r)) => match file.resolve(*r) {
            Ok(PdfObject::Name(n)) => {
                CidCMap::predefined(n.as_str()).unwrap_or_else(|| identity_fallback(n.as_str()))
            }
            Ok(PdfObject::Stream(s)) => {
                let data = file.resolve_stream_data(*r).or_else(|_| {
                    zpdf_parser::filters::decode_stream_with_limits(&s.data, &s.dict, file.limits())
                });
                let mut cmap = match data {
                    Ok(d) => CidCMap::parse(&d),
                    Err(e) => {
                        tracing::warn!("undecodable embedded CMap: {e}; using Identity-H");
                        CidCMap::identity(0)
                    }
                };
                // /WMode may also live on the stream dict.
                if let Ok(wmode) = s.dict.get_i64("WMode") {
                    cmap.apply_stream_wmode(wmode);
                }
                cmap
            }
            _ => CidCMap::identity(0),
        },
        _ => CidCMap::identity(0),
    }
}

/// /DW2 vertical metrics from a CID font dict: [vy w1y], default [880 −1000].
fn parse_dw2(file: &PdfFile, desc_dict: &zpdf_core::PdfDict) -> (f64, f64) {
    resolve_array(file, desc_dict, "DW2")
        .and_then(|arr| {
            let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
            (v.len() >= 2).then(|| (v[0], v[1]))
        })
        .unwrap_or((880.0, -1000.0))
}

fn load_type0_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
    provider: Option<&FontProvider>,
    to_unicode: Option<zpdf_font::cmap::ToUnicodeMap>,
) -> Result<(LoadedFont, zpdf_core::ProcessingDiagnostics)> {
    // /DescendantFonts is commonly an indirect reference to the array.
    let descendants = resolve_array(file, dict, "DescendantFonts")
        .ok_or_else(|| zpdf_core::Error::MissingKey("DescendantFonts".into()))?;
    // /DescendantFonts is commonly an indirect reference to the array; the
    // CIDFont entry itself may also be inlined (AutoCAD) instead of a ref.
    let desc_dict = descendant_cid_dict(file, &descendants)
        .ok_or_else(|| zpdf_core::Error::MissingKey("DescendantFonts[0]".into()))?;
    let desc_dict = &desc_dict;

    let mut cid_widths = parse_cid_widths(file, desc_dict);
    parse_cid_w2(file, desc_dict, &mut cid_widths);
    let mut cmap = parse_type0_encoding(file, dict);
    let mut ordering = None;
    if let Some(info) = resolve_dict(file, desc_dict, "CIDSystemInfo") {
        ordering = match info.get("Ordering") {
            Some(PdfObject::String(value)) => Some(value.to_string_lossy()),
            Some(PdfObject::Name(value)) => Some(value.as_str().to_string()),
            _ => None,
        };
        let adobe = match info.get("Registry") {
            Some(PdfObject::String(value)) => value.0 == b"Adobe",
            Some(PdfObject::Name(value)) => value.as_str() == "Adobe",
            _ => false,
        };
        if adobe {
            if let Some(ordering) = &ordering {
                cmap.set_collection(ordering);
            }
        }
    }
    let dw2 = parse_dw2(file, desc_dict);

    // Reuse the descriptor for font bytes, substitution hints and variations.
    // Resolving the same indirect dictionaries for each stage needlessly
    // reparses and clones them on cold composite-font loading.
    let descriptor = resolve_dict(file, desc_dict, "FontDescriptor");
    let hints = descriptor_hints(file, descriptor.as_ref());
    let lookup_name = substitute_font_name(&base_font, descriptor.as_ref());
    let font_file = descriptor
        .as_ref()
        .map(|fd| extract_font_file(file, fd))
        .unwrap_or(EmbeddedFontFile::Absent);

    let fallback_type0 = |cid_widths: CidWidths| {
        // Non-embedded or unloadable embedded composite font: substitute a
        // matching face. CIDs are remapped through /ToUnicode once it is
        // attached (see build_substitute_cid_to_gid in load_single_font).
        substitute_type0_font(
            lookup_name,
            &base_font,
            cid_widths.clone(),
            provider,
            ordering.as_deref(),
            hints,
        )
        .unwrap_or_else(|| {
            let mut font = LoadedFont::new_placeholder(base_font.clone());
            // Missing outlines do not change the PDF's composite code
            // segmentation, CID widths or vertical positioning.
            font.font_type = PdfFontType::Type0CidType2;
            font.cid_widths = cid_widths;
            font
        })
    };

    // A substituted composite font can only resolve glyphs when codes or CIDs
    // map to Unicode (via a Unicode/legacy CMap, a known Adobe CJK collection,
    // or a parsed non-empty /ToUnicode). Without any of those, substituting
    // would drop every glyph and discard the PDF's /CIDToGIDMap.
    let can_substitute = cmap.codes_are_unicode
        || cmap.is_legacy()
        || cmap.collection().is_some()
        || to_unicode.is_some();

    let try_mapped_substitute = |cid_widths: CidWidths| -> Option<LoadedFont> {
        let mut sub = substitute_type0_font(
            lookup_name,
            &base_font,
            cid_widths,
            provider,
            ordering.as_deref(),
            hints,
        )?;
        sub.cid_cmap = Some(cmap.clone());
        sub.to_unicode = to_unicode.clone();
        sub.validate_cid_cmap();
        sub.build_substitute_cid_to_gid();
        let has_mapping = sub.unicode_coded()
            || sub.legacy_substitute()
            || sub.cid_cmap.as_ref().and_then(|c| c.collection()).is_some()
            || sub
                .cid_to_gid
                .as_ref()
                .is_some_and(|m| m.values().any(|&g| g != 0));
        has_mapping.then_some(sub)
    };

    let mut diagnostics = zpdf_core::ProcessingDiagnostics::default();
    let mut font = match font_file {
        EmbeddedFontFile::Present { object_id, data } => {
            let mut font = LoadedFont::new_with_data(
                PdfFontType::Type0CidType2,
                base_font.clone(),
                data,
                cid_widths.clone(),
            );
            // /CIDToGIDMap stream: explicit CID → GID table, authoritative for
            // CIDFontType2 (TrueType-based) descendants. A raw-CFF CIDFontType0
            // descendant keeps its charset-derived map built in new_with_data —
            // there /CIDToGIDMap is not even a legal key.
            if let Some(map) = parse_cid_to_gid_stream(file, desc_dict) {
                let subtype = desc_dict.get_name("Subtype").unwrap_or("");
                if subtype == "CIDFontType2" {
                    font.cid_to_gid = Some(map);
                }
            }
            // Some embedded CID-keyed CFF subsets are defective and cannot be
            // outlined (unparseable per-FD Private DICTs strand the local subrs),
            // so most glyphs render blank. When the embedded program fails to
            // parse or lacks usable outlines, fall back to a substitute face
            // only if the replacement can actually map CIDs (otherwise keep the
            // original so any surviving embedded glyph still renders).
            let cjk = is_cjk_ordering(ordering.as_deref())
                || zpdf_font::system::cjk_ordering_for(lookup_name).is_some();
            if !font.has_usable_embedded_outlines() {
                tracing::warn!(
                    "font {base_font}: embedded composite program failed to parse; falling back to substitute"
                );
                diagnostics = embedded_fallback_diagnostics(object_id, None);
                if can_substitute {
                    try_mapped_substitute(cid_widths).unwrap_or(font)
                } else {
                    font
                }
            } else if cjk && font.embedded_outline_failure_rate() > 0.5 {
                if can_substitute {
                    if let Some(sub) = try_mapped_substitute(cid_widths) {
                        diagnostics = embedded_fallback_diagnostics(object_id, None);
                        sub
                    } else {
                        font
                    }
                } else {
                    font
                }
            } else {
                font
            }
        }
        EmbeddedFontFile::Unreadable { object_id, error } => {
            tracing::warn!(
                "font {base_font}: embedded composite font stream unreadable; falling back to substitute"
            );
            diagnostics = embedded_fallback_diagnostics(object_id, error.as_ref());
            if can_substitute {
                fallback_type0(cid_widths)
            } else {
                let mut font = LoadedFont::new_placeholder(base_font);
                font.font_type = PdfFontType::Type0CidType2;
                font.cid_widths = cid_widths;
                if let Some(map) = parse_cid_to_gid_stream(file, desc_dict) {
                    if desc_dict.get_name("Subtype").unwrap_or("") == "CIDFontType2" {
                        font.cid_to_gid = Some(map);
                    }
                }
                font
            }
        }
        EmbeddedFontFile::Absent => fallback_type0(cid_widths),
    };
    font.cid_cmap = Some(cmap);
    font.dw2 = dw2;
    // A Unicode-coded CMap is only usable when the font program can resolve
    // Unicode; otherwise fall back to Identity (codes pass through as CIDs).
    font.validate_cid_cmap();
    if let Some(descriptor) = descriptor.as_ref() {
        apply_variations_from_descriptor(descriptor, &mut font);
    }
    Ok((font, diagnostics))
}

/// A registered CJK character-collection ordering (not Adobe-Identity).
fn is_cjk_ordering(ordering: Option<&str>) -> bool {
    matches!(ordering, Some("GB1" | "CNS1" | "Japan1" | "Korea1" | "KR"))
}

/// Build a system-font substitute for a composite (Type0) font, carrying over
/// the PDF's authoritative /W advances. Returns `None` when no installed face
/// matches (caller keeps the embedded font or a placeholder).
fn substitute_type0_font(
    lookup_name: &str,
    base_font: &str,
    cid_widths: CidWidths,
    provider: Option<&FontProvider>,
    ordering: Option<&str>,
    hints: zpdf_font::system::SubstituteHints,
) -> Option<LoadedFont> {
    let matched = match provider {
        Some(provider) => provider.find_font(lookup_name, hints, ordering),
        None => zpdf_font::system::find_system_font(lookup_name, hints, ordering),
    };
    matched.and_then(|m| {
        LoadedFont::new_substitute(
            PdfFontType::Type0CidType2,
            base_font.to_string(),
            m.data,
            m.face_index,
            cid_widths,
        )
    })
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
    // A stream without one complete entry (empty or truncated by a broken
    // producer) states nothing; keep the Identity default.
    if data.len() < 2 {
        tracing::debug!("CIDToGIDMap {stream_ref}: no complete entry, using Identity");
        return None;
    }
    let mut map = std::collections::HashMap::new();
    for (cid, gid_bytes) in data
        .as_chunks::<2>()
        .0
        .iter()
        .enumerate()
        .take(u16::MAX as usize + 1)
    {
        let gid = u16::from_be_bytes([gid_bytes[0], gid_bytes[1]]);
        if gid != 0 {
            map.insert(cid as u16, gid);
        }
    }
    // An explicit all-.notdef map is empty, not an identity mapping.
    Some(map)
}

fn load_simple_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
    font_type: PdfFontType,
    provider: Option<&FontProvider>,
) -> Result<(LoadedFont, zpdf_core::ProcessingDiagnostics)> {
    let cid_widths = parse_simple_widths(file, dict);
    let descriptor = resolve_dict(file, dict, "FontDescriptor");
    let font_file = descriptor
        .as_ref()
        .map(|fd| extract_font_file(file, fd))
        .unwrap_or(EmbeddedFontFile::Absent);

    match font_file {
        EmbeddedFontFile::Present { object_id, data } => {
            let font = LoadedFont::new_with_data(
                font_type.clone(),
                base_font.clone(),
                data,
                cid_widths.clone(),
            );
            if font.has_usable_embedded_outlines() {
                Ok((font, zpdf_core::ProcessingDiagnostics::default()))
            } else {
                tracing::warn!(
                    "font {base_font}: embedded program failed to parse; falling back to substitute"
                );
                let fallback = try_system_substitute_simple(
                    file,
                    descriptor.as_ref(),
                    &base_font,
                    font_type,
                    cid_widths,
                    provider,
                )
                .unwrap_or(font);
                Ok((fallback, embedded_fallback_diagnostics(object_id, None)))
            }
        }
        EmbeddedFontFile::Unreadable { object_id, error } => {
            tracing::warn!(
                "font {base_font}: embedded font stream unreadable; falling back to substitute"
            );
            let fallback = fallback_simple_font(
                file,
                descriptor.as_ref(),
                base_font,
                font_type,
                cid_widths,
                provider,
            );
            Ok((
                fallback,
                embedded_fallback_diagnostics(object_id, error.as_ref()),
            ))
        }
        EmbeddedFontFile::Absent => Ok((
            fallback_simple_font(
                file,
                descriptor.as_ref(),
                base_font,
                font_type,
                cid_widths,
                provider,
            ),
            zpdf_core::ProcessingDiagnostics::default(),
        )),
    }
}

fn type3_differences(diffs: &[PdfObject]) -> Vec<String> {
    let mut encoding = Vec::new();
    let mut current = Some(0usize);
    for obj in diffs {
        match obj {
            PdfObject::Integer(n) => {
                current = usize::try_from(*n).ok().filter(|&code| code <= 255);
            }
            PdfObject::Name(name) => {
                let Some(code) = current else { continue };
                // A later run may restart at a lower code; never shrink.
                if encoding.len() <= code {
                    encoding.resize(code + 1, String::new());
                }
                encoding[code] = name.0.clone();
                current = code.checked_add(1).filter(|&next| next <= 255);
            }
            _ => {}
        }
    }
    encoding
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
            encoding = type3_differences(&diffs);
        }
    }

    // CharProcs: name → stream ref
    let mut char_procs = std::collections::HashMap::new();
    let mut decoded: std::collections::HashMap<ObjectId, Arc<[u8]>> =
        std::collections::HashMap::new();
    let mut remaining = file.limits().max_font_cache_bytes;
    let encoded_names: std::collections::HashSet<&str> = encoding
        .iter()
        .map(String::as_str)
        .filter(|name| !name.is_empty())
        .collect();
    if let Some(cp_dict) = resolve_dict(file, dict, "CharProcs") {
        for (name, obj) in &cp_dict.0 {
            if char_procs.len() >= encoded_names.len() {
                break;
            }
            if !encoded_names.contains(name.as_str()) {
                continue;
            }
            if let PdfObject::Ref(r) = obj {
                let data = if let Some(data) = decoded.get(r) {
                    Arc::clone(data)
                } else {
                    // Bound the next unique program before decoding, while
                    // already-admitted programs remain live. Aliased glyph
                    // names share one decoded buffer and consume no bytes twice.
                    let data = match file.resolve_stream_data_with_limit(*r, remaining) {
                        Ok(data) => data,
                        Err(error) => {
                            tracing::debug!("Type3 glyph {name}: skipped - {error}");
                            continue;
                        }
                    };
                    remaining = remaining.checked_sub(data.len() as u64).ok_or(
                        zpdf_core::Error::StreamSizeLimit(file.limits().max_font_cache_bytes),
                    )?;
                    let data: Arc<[u8]> = Arc::from(data);
                    decoded.insert(*r, Arc::clone(&data));
                    data
                };
                char_procs.insert(name.0.clone(), data);
            }
        }
    }

    // Widths
    let first_char = dict
        .get_i64("FirstChar")
        .ok()
        .and_then(|n| u8::try_from(n).ok())
        .map(u16::from)
        .unwrap_or(0);
    let widths: Vec<f64> = resolve_array(file, dict, "Widths")
        .unwrap_or_default()
        .iter()
        .take(256)
        .map(|o| o.as_f64().unwrap_or(0.0))
        .collect();
    // Codes outside /Widths advance by /MissingWidth (0 by default), in glyph
    // space like /Widths. Without a /Widths array at all, keep the 1000
    // fallback rather than collapsing every glyph onto the first.
    let default_width = if widths.is_empty() {
        1000.0
    } else {
        resolve_dict(file, dict, "FontDescriptor").map_or(0.0, |fd| missing_width(file, &fd))
    };

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
        face_index: 0,
        is_substitute: false,
        cid_widths: CidWidths::new(default_width),
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
        cid_cmap: None,
        dw2: (880.0, -1000.0),
        variations: Vec::new(),
    };

    Ok(font)
}

enum EmbeddedFontFile {
    /// Descriptor declares no `/FontFile`, `/FontFile2`, or `/FontFile3`.
    Absent,
    /// Embedded program decoded to non-empty bytes.
    Present {
        object_id: Option<ObjectId>,
        data: Vec<u8>,
    },
    /// Descriptor declares an embedded font stream, but resolving/decoding it
    /// failed or produced an empty/non-stream object.
    Unreadable {
        object_id: Option<ObjectId>,
        error: Option<zpdf_core::Error>,
    },
}

/// Extract the embedded program using an already resolved FontDescriptor,
/// distinguishing absent streams (normal non-embedded fonts) from declared
/// streams that failed to decode or were empty.
fn extract_font_file(file: &PdfFile, fd_dict: &zpdf_core::PdfDict) -> EmbeddedFontFile {
    let mut first_failure: Option<(Option<ObjectId>, Option<zpdf_core::Error>)> = None;
    // Try FontFile2 (TrueType), then FontFile3 (OpenType/CFF), then FontFile (Type1)
    for key in &["FontFile2", "FontFile3", "FontFile"] {
        match fd_dict.get(key) {
            None | Some(PdfObject::Null) => continue,
            Some(PdfObject::Ref(ff_ref)) => match file.resolve_stream_data(*ff_ref) {
                Ok(data) if !data.is_empty() => {
                    return EmbeddedFontFile::Present {
                        object_id: Some(*ff_ref),
                        data,
                    };
                }
                Ok(_) => {
                    first_failure.get_or_insert((Some(*ff_ref), None));
                }
                Err(err) => {
                    first_failure.get_or_insert((Some(*ff_ref), Some(err)));
                }
            },
            Some(_) => {
                first_failure.get_or_insert((None, None));
            }
        }
    }
    match first_failure {
        Some((object_id, error)) => EmbeddedFontFile::Unreadable { object_id, error },
        None => EmbeddedFontFile::Absent,
    }
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

/// Per-CID metrics a /W or /W2 array may assign in total. A range entry
/// covers up to 65,536 CIDs and a list entry's (shared, indirect) sub-array as
/// many, while the array's entry count is unbounded; repeated entries would
/// otherwise re-assign the whole CID space each.
const MAX_CID_METRIC_ASSIGNMENTS: usize = 4 * 65_536;

/// Parse CID /W array: format is [cid [w1 w2 ...]] or [cid_first cid_last w]
fn parse_cid_widths(file: &PdfFile, dict: &zpdf_core::PdfDict) -> CidWidths {
    let dw = dict.get_f64("DW").unwrap_or(1000.0);
    let mut widths = CidWidths::new(dw);

    let w_array = match resolve_array(file, dict, "W") {
        Some(arr) => arr,
        None => return widths,
    };

    let mut budget = MAX_CID_METRIC_ASSIGNMENTS;
    let mut i = 0;
    while i < w_array.len() && budget > 0 {
        let cid_start = match w_array[i].as_i64().ok().and_then(|v| u16::try_from(v).ok()) {
            Some(v) => v,
            None => break,
        };
        i += 1;
        if i >= w_array.len() {
            break;
        }

        // In the `[cid [w1 w2 ...]]` form the width sub-array is frequently an
        // *indirect* reference (AutoCAD/nanoCAD export /W this way). Resolve one
        // level so the `PdfObject::Array` arm below sees it; otherwise the entry
        // fell through to `_ => i += 1`, the sub-array was skipped, and every CID
        // in the range silently defaulted to /DW — breaking advances (and thus
        // inter-glyph spacing), so the text drifts.
        let resolved = if let PdfObject::Ref(id) = &w_array[i] {
            file.resolve(*id).ok()
        } else {
            None
        };
        let entry = resolved.as_ref().unwrap_or(&w_array[i]);

        match entry {
            PdfObject::Array(arr) => {
                // [cid_start [w1 w2 w3 ...]]
                let count = arr.len().min(budget);
                budget -= count;
                for (j, obj) in arr[..count].iter().enumerate() {
                    let Some(cid) = u16::try_from(j)
                        .ok()
                        .and_then(|delta| cid_start.checked_add(delta))
                    else {
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
                let cid_end = entry
                    .as_i64()
                    .ok()
                    .and_then(|v| u16::try_from(v).ok())
                    .unwrap_or(cid_start);
                i += 1;
                if i < w_array.len() {
                    let w = w_array[i].as_f64().unwrap_or(dw);
                    for cid in (cid_start..=cid_end).take(budget) {
                        widths.set(cid, w);
                        budget -= 1;
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

/// Parse the CID /W2 array (PDF 9.7.4.3) into per-CID vertical metrics.
/// Two element forms, mirroring /W but with THREE numbers per glyph:
///   `c [ w1y_1 vx_1 vy_1  w1y_2 vx_2 vy_2 ... ]`   (list form)
///   `cFirst cLast w1y vx vy`                         (range form)
/// where `w1y` is the vertical displacement and `(vx, vy)` the position vector.
fn parse_cid_w2(file: &PdfFile, dict: &zpdf_core::PdfDict, widths: &mut CidWidths) {
    if let Some(arr) = resolve_array(file, dict, "W2") {
        apply_w2_array(&arr, widths);
    }
}

fn apply_w2_array(w2_array: &[PdfObject], widths: &mut CidWidths) {
    let mut budget = MAX_CID_METRIC_ASSIGNMENTS;
    let mut i = 0;
    while i < w2_array.len() && budget > 0 {
        let cid_start = match w2_array[i]
            .as_i64()
            .ok()
            .and_then(|v| u16::try_from(v).ok())
        {
            Some(v) => v,
            None => break,
        };
        i += 1;
        if i >= w2_array.len() {
            break;
        }

        match &w2_array[i] {
            PdfObject::Array(arr) => {
                // List form: triples (w1y, vx, vy) starting at cid_start.
                let mut k = 0;
                while k + 2 < arr.len() && budget > 0 {
                    budget -= 1;
                    let (Ok(w1y), Ok(vx), Ok(vy)) =
                        (arr[k].as_f64(), arr[k + 1].as_f64(), arr[k + 2].as_f64())
                    else {
                        break;
                    };
                    let Some(cid) = u16::try_from(k / 3)
                        .ok()
                        .and_then(|delta| cid_start.checked_add(delta))
                    else {
                        break;
                    };
                    widths.set_v(cid, w1y, vx, vy);
                    k += 3;
                }
                i += 1;
            }
            PdfObject::Integer(_) | PdfObject::Real(_) => {
                // Range form: cFirst cLast w1y vx vy.
                let cid_end = w2_array[i]
                    .as_i64()
                    .ok()
                    .and_then(|v| u16::try_from(v).ok())
                    .unwrap_or(cid_start);
                if i + 3 < w2_array.len() {
                    let (Ok(w1y), Ok(vx), Ok(vy)) = (
                        w2_array[i + 1].as_f64(),
                        w2_array[i + 2].as_f64(),
                        w2_array[i + 3].as_f64(),
                    ) else {
                        break;
                    };
                    for cid in (cid_start..=cid_end).take(budget) {
                        widths.set_v(cid, w1y, vx, vy);
                        budget -= 1;
                    }
                    i += 4;
                } else {
                    break;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
}

fn parse_simple_widths(file: &PdfFile, dict: &zpdf_core::PdfDict) -> CidWidths {
    let first_char = dict
        .get_i64("FirstChar")
        .ok()
        .and_then(|v| u8::try_from(v).ok())
        .map(u16::from)
        .unwrap_or(0);
    let widths_array = resolve_array(file, dict, "Widths");
    // Codes the /Widths array does not cover take the descriptor's
    // /MissingWidth, which defaults to 0 (Table 122). Fonts without /Widths or
    // without a descriptor (the standard 14 among them) keep the 1000
    // fallback; their advances come from built-in metrics or the program.
    let (default_width, authoritative_default) =
        match (&widths_array, resolve_dict(file, dict, "FontDescriptor")) {
            (Some(_), Some(fd)) => (missing_width(file, &fd), true),
            _ => (1000.0, false),
        };
    let mut widths = CidWidths::new(default_width);
    widths.set_authoritative_default(authoritative_default);

    if let Some(arr) = widths_array {
        for (j, obj) in arr.iter().take(256).enumerate() {
            let Some(code) = u16::try_from(j)
                .ok()
                .and_then(|delta| first_char.checked_add(delta))
                .filter(|&code| code <= 255)
            else {
                break;
            };
            if let Ok(w) = obj.as_f64() {
                widths.set(code, w);
            }
        }
    }

    widths
}

/// A font descriptor's `/MissingWidth`: the advance of codes `/Widths` does
/// not cover, 0 when absent (Table 122).
fn missing_width(file: &PdfFile, descriptor: &zpdf_core::PdfDict) -> f64 {
    match descriptor.get("MissingWidth") {
        Some(PdfObject::Ref(id)) => file.resolve(*id).ok().and_then(|o| o.as_f64().ok()),
        Some(other) => other.as_f64().ok(),
        None => None,
    }
    .filter(|w| w.is_finite())
    .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(v: i64) -> PdfObject {
        PdfObject::Integer(v)
    }
    fn real(v: f64) -> PdfObject {
        PdfObject::Real(v)
    }

    #[test]
    fn w2_list_form_assigns_consecutive_cids() {
        // 120 [w1y vx vy  w1y vx vy] → CIDs 120 and 121.
        let arr = vec![
            int(120),
            PdfObject::Array(vec![
                real(-1000.0),
                real(500.0),
                real(880.0),
                int(-900),
                int(450),
                int(820),
            ]),
        ];
        let mut w = CidWidths::new(1000.0);
        apply_w2_array(&arr, &mut w);
        assert_eq!(w.get_v(120), Some((-1000.0, 500.0, 880.0)));
        assert_eq!(w.get_v(121), Some((-900.0, 450.0, 820.0)));
        assert_eq!(w.get_v(122), None);
    }

    #[test]
    fn w2_range_form_assigns_inclusive_range() {
        // cFirst cLast w1y vx vy
        let arr = vec![int(10), int(12), int(-1000), int(500), int(880)];
        let mut w = CidWidths::new(1000.0);
        apply_w2_array(&arr, &mut w);
        for cid in 10..=12 {
            assert_eq!(w.get_v(cid), Some((-1000.0, 500.0, 880.0)));
        }
        assert_eq!(w.get_v(9), None);
        assert_eq!(w.get_v(13), None);
    }

    #[test]
    fn w2_repeated_full_ranges_stop_at_the_assignment_budget() {
        let mut arr = Vec::new();
        for _ in 0..8 {
            arr.extend([int(0), int(65_535), int(-1000), int(500), int(880)]);
        }
        // Past the budget: never applied.
        arr.extend([int(7), int(7), int(-1), int(1), int(1)]);
        let mut w = CidWidths::new(1000.0);
        apply_w2_array(&arr, &mut w);
        assert_eq!(w.get_v(7), Some((-1000.0, 500.0, 880.0)));
    }

    #[test]
    fn w2_truncated_entry_is_ignored_not_panic() {
        // Range header without the trailing metric numbers must not panic.
        let arr = vec![int(10), int(12), int(-1000)];
        let mut w = CidWidths::new(1000.0);
        apply_w2_array(&arr, &mut w);
        assert_eq!(w.get_v(10), None);
    }

    #[test]
    fn type3_programs_use_the_remaining_budget_and_share_references() {
        let pdf = crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>",
            "<< /Type /Font /Subtype /Type3 /Encoding << /Differences [65 /A /B /C] >> /CharProcs << /A 3 0 R /B 3 0 R /C 4 0 R >> >>",
            "<< /Length 3 >>\nstream\nq Q\nendstream",
            "<< /Length 3 >>\nstream\nq Q\nendstream",
        ]);
        let limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: 3,
            ..Default::default()
        };
        let file = PdfFile::parse_with_limits(pdf.clone(), limits).unwrap();
        let font = load_single_font(&file, ObjectId(2, 0)).unwrap();
        let PdfFontType::Type3 { char_procs, .. } = &font.font_type else {
            panic!("Type3")
        };
        assert_eq!(char_procs.len(), 2);
        assert!(std::sync::Arc::ptr_eq(&char_procs["A"], &char_procs["B"]));
        assert_eq!(char_procs["A"].as_ref(), b"q Q");
        assert!(!char_procs.contains_key("C"));
        let limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: 6,
            ..Default::default()
        };
        let file = PdfFile::parse_with_limits(pdf, limits).unwrap();
        let font = load_single_font(&file, ObjectId(2, 0)).unwrap();
        let PdfFontType::Type3 { char_procs, .. } = &font.font_type else {
            panic!("Type3")
        };
        assert_eq!(char_procs.len(), 3);
        assert!(!std::sync::Arc::ptr_eq(&char_procs["A"], &char_procs["C"]));
    }

    #[test]
    fn type3_differences_reject_out_of_range_codes_without_growth() {
        let name = |s: &str| PdfObject::Name(zpdf_core::PdfName::new(s));
        let diffs = vec![
            int(-1),
            name("ignored-negative"),
            int(i64::MAX),
            name("ignored-huge"),
            int(255),
            name("last"),
            name("past-end"),
        ];
        let encoding = type3_differences(&diffs);
        assert_eq!(encoding.len(), 256);
        assert_eq!(encoding[255], "last");
    }

    #[test]
    fn type3_differences_keep_higher_codes_when_a_run_restarts_lower() {
        let name = |s: &str| PdfObject::Name(zpdf_core::PdfName::new(s));
        let diffs = vec![int(65), name("A"), int(32), name("space")];
        let encoding = type3_differences(&diffs);
        assert_eq!(encoding[65], "A");
        assert_eq!(encoding[32], "space");
    }

    #[test]
    fn simple_differences_do_not_overflow_after_huge_code() {
        let mut dict = zpdf_core::PdfDict::new();
        dict.insert(
            zpdf_core::PdfName::new("Differences"),
            PdfObject::Array(vec![
                int(i64::MAX),
                PdfObject::Name(zpdf_core::PdfName::new("ignored")),
            ]),
        );
        let mut encoding =
            zpdf_font::encoding::Encoding::from_base(&zpdf_font::encoding::STANDARD_ENCODING);
        apply_differences(&dict, &mut encoding);
    }

    /// Type 3 codes outside /Widths advance by /MissingWidth (0 by default),
    /// not by 1000 glyph-space units, which a FontMatrix other than 0.001
    /// would turn into a many-em gap.
    #[test]
    fn type3_codes_outside_widths_take_missing_width() {
        let file = PdfFile::parse(crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>",
            "<< /Type /Font /Subtype /Type3 /FontMatrix [0.01 0 0 0.01 0 0] \
             /FontBBox [0 0 100 100] /CharProcs << >> /FirstChar 65 /Widths [60] >>",
            "<< /Type /Font /Subtype /Type3 /FontMatrix [0.01 0 0 0.01 0 0] \
             /FontBBox [0 0 100 100] /CharProcs << >> /FirstChar 65 /Widths [60] \
             /FontDescriptor 4 0 R >>",
            "<< /Type /FontDescriptor /MissingWidth 25 >>",
        ]))
        .expect("parse");
        let width = |n| {
            load_single_font_with_provider(&file, ObjectId(n, 0), None)
                .expect("type3 font")
                .type3_glyph_width(66)
        };
        assert_eq!(width(2), 0.0);
        assert_eq!(width(3), 25.0);
    }

    /// Codes outside /Widths take the descriptor's /MissingWidth (0 when the
    /// descriptor omits it); without /Widths or a descriptor the 1000 fallback
    /// is kept.
    #[test]
    fn simple_widths_default_to_missing_width() {
        use zpdf_core::{PdfDict, PdfName};
        let file = PdfFile::parse(crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>",
            "<< /Type /FontDescriptor /MissingWidth 333 >>",
            "<< /Type /FontDescriptor >>",
        ]))
        .expect("parse");
        let font = |widths: bool, descriptor: Option<u32>| {
            let mut d = PdfDict::new();
            d.insert(PdfName::new("FirstChar"), int(65));
            if widths {
                d.insert(
                    PdfName::new("Widths"),
                    PdfObject::Array(vec![int(500), int(600)]),
                );
            }
            if let Some(n) = descriptor {
                d.insert(
                    PdfName::new("FontDescriptor"),
                    PdfObject::Ref(ObjectId(n, 0)),
                );
            }
            parse_simple_widths(&file, &d)
        };

        let w = font(true, Some(2));
        assert_eq!((w.get(65), w.get(66)), (500.0, 600.0));
        assert_eq!(w.get(64), 333.0, "below /FirstChar");
        assert_eq!(w.get(67), 333.0, "past /LastChar");
        assert_eq!(font(true, Some(3)).get(67), 0.0, "spec default");
        assert_eq!(font(true, None).get(67), 1000.0);
        assert_eq!(font(false, Some(2)).get(67), 1000.0);
    }

    #[test]
    fn descriptor_hints_read_flags_stemv_weight_and_italic_angle() {
        let file = PdfFile::parse(crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>",
            "<< /Type /FontDescriptor /Flags 3 /FontWeight 700 /ItalicAngle -12 >>",
            "<< /Type /FontDescriptor /Flags 32 /FontWeight 400 /ItalicAngle 0 /StemV 80 >>",
            "<< /Type /FontDescriptor /Flags 32 /FontWeight 5 0 R >>",
            "600",
        ]))
        .expect("parse");

        let d1 = file.resolve(ObjectId(2, 0)).unwrap();
        let h1 = descriptor_hints(&file, Some(d1.as_dict().unwrap()));
        assert!(h1.fixed_pitch);
        assert!(h1.serif);
        assert!(h1.bold);
        assert!(h1.italic);

        let d2 = file.resolve(ObjectId(3, 0)).unwrap();
        let h2 = descriptor_hints(&file, Some(d2.as_dict().unwrap()));
        assert!(!h2.fixed_pitch && !h2.serif && !h2.bold && !h2.italic);

        let d3 = file.resolve(ObjectId(4, 0)).unwrap();
        let h3 = descriptor_hints(&file, Some(d3.as_dict().unwrap()));
        assert!(h3.bold, "indirect /FontWeight 600 should set bold hint");
    }
}
