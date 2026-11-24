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

    // Admit primary page fonts and park any fitting embedded companion as a
    // budget-charged, reclaimable standby copy alongside its lightweight
    // `ObjectId` reload source. If the companion exceeds `max_bytes` it is
    // dropped immediately; if a later primary page font, Form font, or drawn
    // companion needs the space, `FontCache` reclaims unused standby copies
    // automatically. Keeping fitting companions in reclaimable standby also
    // preserves mixed-CID recovery for callers that pass only `load_page_fonts`
    // to `ContentInterpreter::with_fonts` without a `PdfFile`.
    for (name, &font_ref) in &page.resources.fonts {
        match shared_or_load(file, font_ref, shared, provider) {
            Ok((font, diagnostics, companion, descriptor_spec)) => {
                match cache.admit_with_diagnostics(
                    name.clone(),
                    font,
                    diagnostics,
                    Some(font_ref),
                    max_bytes,
                ) {
                    Ok(fid) => {
                        if cache.get(fid).is_some_and(|f| f.has_font_data()) {
                            if let Some(spec) = descriptor_spec {
                                cache.record_descriptor_substitute(fid, spec, max_bytes);
                            } else if companion.is_some() {
                                let _ = cache.record_companion(
                                    fid,
                                    zpdf_font::CompanionKind::EmbeddedOriginal,
                                    zpdf_font::CompanionSource::Object(font_ref),
                                    companion,
                                    max_bytes,
                                );
                            }
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            "font cache byte limit ({max_bytes}) or ID capacity reached; could not admit {name}"
                        );
                    }
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
) -> Result<crate::SharedFontEntry> {
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
) -> Result<crate::SharedFontEntry> {
    if let Some(font) = shared.get(font_ref) {
        return Ok(font);
    }
    let (font, diagnostics, companion, embedded_file_id) =
        load_single_font_with_fallback(file, font_ref, provider.map(|p| p.as_ref()))?;
    Ok(shared.admit(
        font_ref,
        font,
        diagnostics,
        companion,
        embedded_file_id,
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
    let (font, diagnostics, _, _) = load_single_font_with_fallback(file, font_ref, provider)?;
    Ok((font, diagnostics))
}

/// Parsed primary font, load-time fallback diagnostics, optional paired
/// embedded-original companion face, and optional descriptor metadata for lazy
/// per-glyph substitution when an embedded program was kept as primary.
#[doc(hidden)]
pub type LoadedFontFallback = (
    LoadedFont,
    zpdf_core::ProcessingDiagnostics,
    Option<LoadedFont>,
    Option<std::sync::Arc<zpdf_font::DescriptorSubstituteSpec>>,
);

/// Like [`load_single_font_with_diagnostics`], also returning any paired
/// companion face and the embedded `/FontFile*` object id when an embedded
/// program was kept as primary.
#[doc(hidden)]
pub fn load_single_font_with_fallback(
    file: &PdfFile,
    font_ref: ObjectId,
    provider: Option<&FontProvider>,
) -> Result<LoadedFontFallback> {
    let obj = file.resolve(font_ref)?;
    let dict = obj.as_dict()?;
    let (font, mut diagnostics, companion, embedded_file_id) =
        load_single_font_dict_with_fallback(file, dict, provider)?;
    if !diagnostics.is_empty() {
        diagnostics.first_failed_object.get_or_insert(font_ref);
    }
    Ok((font, diagnostics, companion, embedded_file_id))
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
    let (font, diagnostics, _, _) = load_single_font_dict_with_fallback(file, dict, provider)?;
    Ok((font, diagnostics))
}

/// Like [`load_single_font_dict_with_diagnostics`], also returning any paired
/// companion face and the embedded `/FontFile*` object id when an embedded
/// program was kept as primary.
#[doc(hidden)]
pub fn load_single_font_dict_with_fallback(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    provider: Option<&FontProvider>,
) -> Result<LoadedFontFallback> {
    let subtype = dict.get_name("Subtype").unwrap_or("");
    let base_font = dict.get_name("BaseFont").unwrap_or("Unknown").to_string();

    let to_unicode = parse_to_unicode(file, dict);
    let (mut font, diagnostics, mut companion, embedded_file_id) = match subtype {
        "Type0" => load_type0_font(file, dict, base_font, provider, to_unicode.as_ref())?,
        "TrueType" => load_simple_font(file, dict, base_font, PdfFontType::TrueType, provider)?,
        "Type3" => (
            load_type3_font(file, dict, base_font)?,
            zpdf_core::ProcessingDiagnostics::default(),
            None,
            None,
        ),
        "Type1" | "MMType1" => {
            load_simple_font(file, dict, base_font, PdfFontType::Type1, provider)?
        }
        _ => (
            LoadedFont::new_placeholder(base_font),
            zpdf_core::ProcessingDiagnostics::default(),
            None,
            None,
        ),
    };

    // Only clone /ToUnicode when a companion is paired alongside the primary;
    // healthy font loads move it once.
    let companion_to_unicode = if companion.is_some() {
        to_unicode.clone()
    } else {
        None
    };
    attach_text_mappings(file, dict, subtype, to_unicode, &mut font);
    // A substituted composite font that does not use direct per-source-code
    // GID resolution needs /ToUnicode (attached just above) to route CIDs
    // through the system face's Unicode cmap.
    if font.cid_to_gid.is_none() && !font.uses_direct_composite_gids() {
        font.build_substitute_cid_to_gid();
    }
    // FontDescriptor weight/width/slant → variation axes (variable fonts only).
    if subtype != "Type0" {
        apply_descriptor_variations(file, dict, &mut font);
    }
    if let Some(ref mut comp) = companion {
        attach_text_mappings(file, dict, subtype, companion_to_unicode, comp);
        if font.is_substitute && comp.type1.is_some() {
            font.inherit_type1_encoding_from(comp);
        }
        if comp.is_substitute && comp.cid_to_gid.is_none() && !comp.uses_direct_composite_gids() {
            comp.build_substitute_cid_to_gid();
        }
        if subtype != "Type0" {
            apply_descriptor_variations(file, dict, comp);
        }
    }
    Ok((font, diagnostics, companion, embedded_file_id))
}

/// Reload only the parseable embedded companion font for an indirect font
/// object when the embedded stream fits within `max_bytes`.
#[doc(hidden)]
pub fn load_embedded_companion_ref(
    file: &PdfFile,
    font_ref: ObjectId,
    max_bytes: u64,
) -> Option<LoadedFont> {
    if max_bytes == 0 {
        return None;
    }
    let obj = file.resolve(font_ref).ok()?;
    let dict = obj.as_dict().ok()?;
    load_embedded_companion_dict(file, dict, max_bytes)
}

/// Reload only the parseable embedded companion font for an inline font
/// dictionary stored under an indirect Form XObject's `/Resources /Font`.
#[doc(hidden)]
pub fn load_embedded_companion_form_inline(
    file: &PdfFile,
    form_id: ObjectId,
    font_name: &str,
    max_bytes: u64,
) -> Option<LoadedFont> {
    if max_bytes == 0 {
        return None;
    }
    let form_obj = file.resolve(form_id).ok()?;
    let form_dict = match &form_obj {
        PdfObject::Stream(stream) => &stream.dict,
        PdfObject::Dict(dict) => dict,
        _ => return None,
    };
    let resources = resolve_dict(file, form_dict, "Resources")?;
    let fonts = resolve_dict(file, &resources, "Font")?;
    let PdfObject::Dict(font_dict) = fonts.get(font_name)? else {
        return None;
    };
    load_embedded_companion_dict(file, font_dict, max_bytes)
}

/// Reload only the parseable embedded companion font for an inline font
/// dictionary stored directly under `/Font /<name>` of an indirect
/// `/Resources` dictionary object when its decoded font stream fits within
/// `max_bytes`.
#[doc(hidden)]
pub fn load_embedded_companion_resources_inline(
    file: &PdfFile,
    resources_id: ObjectId,
    font_name: &str,
    max_bytes: u64,
) -> Option<LoadedFont> {
    if max_bytes == 0 {
        return None;
    }
    let res_obj = file.resolve(resources_id).ok()?;
    let res_dict = res_obj.as_dict().ok()?;
    let fonts = resolve_dict(file, res_dict, "Font")?;
    let PdfObject::Dict(font_dict) = fonts.get(font_name)? else {
        return None;
    };
    load_embedded_companion_dict(file, font_dict, max_bytes)
}

/// Reload only the parseable embedded companion font for a font dictionary
/// when its decoded font stream fits within `max_bytes`.
#[doc(hidden)]
pub fn load_embedded_companion_dict(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    max_bytes: u64,
) -> Option<LoadedFont> {
    if max_bytes == 0 {
        return None;
    }
    let subtype = dict.get_name("Subtype").unwrap_or("");
    let base_font = dict.get_name("BaseFont").unwrap_or("Unknown").to_string();
    let mut font = match subtype {
        "Type0" => {
            let descendants = resolve_array(file, dict, "DescendantFonts")?;
            let desc_dict = descendant_cid_dict(file, &descendants)?;
            let descriptor = resolve_dict(file, &desc_dict, "FontDescriptor")?;
            let EmbeddedFontFile::Present { data, .. } =
                extract_font_file_with_limit(file, &descriptor, Some(max_bytes))
            else {
                return None;
            };
            let mut cid_widths = parse_cid_widths(file, &desc_dict);
            parse_cid_w2(file, &desc_dict, &mut cid_widths);
            let mut font =
                LoadedFont::new_with_data(PdfFontType::Type0CidType2, base_font, data, cid_widths);
            if !font.has_font_data() {
                return None;
            }
            if desc_dict.get_name("Subtype").unwrap_or("") == "CIDFontType2" {
                if let Some(map) = parse_cid_to_gid_stream(file, &desc_dict) {
                    font.cid_to_gid = Some(map);
                }
            }
            let (cmap, _) = parse_type0_cmap_and_ordering(file, dict, &desc_dict);
            font.cid_cmap = Some(cmap);
            font.dw2 = parse_dw2(file, &desc_dict);
            font.validate_cid_cmap();
            apply_variations_from_descriptor(&descriptor, &mut font);
            font
        }
        "TrueType" | "Type1" | "MMType1" => {
            let font_type = if subtype == "TrueType" {
                PdfFontType::TrueType
            } else {
                PdfFontType::Type1
            };
            let descriptor = resolve_dict(file, dict, "FontDescriptor")?;
            let EmbeddedFontFile::Present { data, .. } =
                extract_font_file_with_limit(file, &descriptor, Some(max_bytes))
            else {
                return None;
            };
            let cid_widths = parse_simple_widths(file, dict);
            let font = LoadedFont::new_with_data(font_type, base_font, data, cid_widths);
            if !font.has_font_data() {
                return None;
            }
            font
        }
        _ => return None,
    };
    let to_unicode = parse_to_unicode(file, dict);
    attach_text_mappings(file, dict, subtype, to_unicode, &mut font);
    if subtype != "Type0" {
        apply_descriptor_variations(file, dict, &mut font);
    }
    Some(font)
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

/// Standard-14 metrics fallback for simple fonts when no system/provider face
/// is available and no parsed embedded program remains. Preserves any explicit
/// PDF `/Widths` or authoritative `/MissingWidth` default.
fn standard_simple_font(base_font: String, cid_widths: CidWidths) -> Option<LoadedFont> {
    let mut standard = LoadedFont::new_standard(base_font)?;
    if !cid_widths.is_empty() || cid_widths.has_authoritative_default() {
        standard.cid_widths = cid_widths;
    }
    Some(standard)
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
    .or_else(|| standard_simple_font(base_font.clone(), cid_widths.clone()))
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
            let base_obj = match enc_dict.get("BaseEncoding") {
                Some(PdfObject::Ref(r)) => file.resolve(*r).ok(),
                other => other.cloned(),
            };
            let base = match base_obj {
                Some(PdfObject::Name(n)) => base_encoding_by_name(n.as_str()),
                _ => None,
            }
            .unwrap_or_else(|| default_simple_base(subtype));
            let mut encoding = Encoding::from_base(base);
            apply_differences(Some(file), &enc_dict, &mut encoding);
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

fn apply_differences(
    file: Option<&PdfFile>,
    enc_dict: &zpdf_core::PdfDict,
    encoding: &mut zpdf_font::encoding::Encoding,
) {
    let diffs_obj = match enc_dict.get("Differences") {
        Some(PdfObject::Ref(r)) => file.and_then(|f| f.resolve(*r).ok()),
        other => other.cloned(),
    };
    if let Some(PdfObject::Array(diffs)) = diffs_obj {
        let mut code = Some(0u16);
        for obj in &diffs {
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

fn parse_type0_cmap_and_ordering(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    desc_dict: &zpdf_core::PdfDict,
) -> (zpdf_font::cmap::CidCMap, Option<String>) {
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
    (cmap, ordering)
}

fn load_type0_font(
    file: &PdfFile,
    dict: &zpdf_core::PdfDict,
    base_font: String,
    provider: Option<&FontProvider>,
    to_unicode: Option<&zpdf_font::cmap::ToUnicodeMap>,
) -> Result<LoadedFontFallback> {
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
    let (cmap, ordering) = parse_type0_cmap_and_ordering(file, dict, desc_dict);
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

    // When the CMap is non-identity or carries explicit CID overrides, multiple
    // source codes can share one CID; resolve physical drawing GIDs per source
    // code instead of a CID-keyed `cid_to_gid` table (while pure Identity-H/V
    // maps are 1-to-1 and keep `cid_to_gid` for direct CID lookups).
    let needs_direct_gids = !cmap.identity || cmap.has_explicit_cids();

    let fallback_type0 = |cid_widths: CidWidths| {
        // Non-embedded or unloadable embedded composite font: substitute a
        // matching face while keeping original PDF CID advances.
        substitute_type0_font(
            lookup_name,
            &base_font,
            cid_widths.clone(),
            provider,
            ordering.as_deref(),
            hints,
        )
        .map(|mut sub| {
            if needs_direct_gids {
                sub.enable_direct_composite_gids();
            }
            sub
        })
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
        sub.validate_cid_cmap();
        if needs_direct_gids {
            sub.enable_direct_composite_gids();
            sub.has_direct_composite_mapping(to_unicode).then_some(sub)
        } else {
            sub.build_substitute_cid_to_gid_from(to_unicode);
            let has_mapping = sub.unicode_coded()
                || sub.legacy_substitute()
                || sub.cid_cmap.as_ref().and_then(|c| c.collection()).is_some()
                || sub
                    .cid_to_gid
                    .as_ref()
                    .is_some_and(|m| m.values().any(|&g| g != 0));
            has_mapping.then_some(sub)
        }
    };

    let mut diagnostics = zpdf_core::ProcessingDiagnostics::default();
    let mut companion: Option<LoadedFont> = None;
    let mut descriptor_spec: Option<std::sync::Arc<zpdf_font::DescriptorSubstituteSpec>> = None;
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
            // Use one consistent sampling policy (`probe_embedded_outlines`) for
            // choosing the primary face: switch primary to a mapped substitute
            // only when the embedded program is `Unusable` (keeping the
            // parseable embedded face as a companion for surviving CIDs). For
            // `PartiallyCorrupt` and `Healthy`, keep the embedded face primary
            // and retain lightweight descriptor metadata so any unreadable
            // shown glyph substitutes lazily without replacing healthy embedded
            // outlines.
            match font.probe_embedded_outlines() {
                zpdf_font::EmbeddedOutlineProbe::Unusable => {
                    tracing::warn!(
                        "font {base_font}: embedded composite program failed to parse; falling back to substitute"
                    );
                    diagnostics = embedded_fallback_diagnostics(object_id, None);
                    if can_substitute {
                        match try_mapped_substitute(cid_widths) {
                            Some(sub) => {
                                if font.has_font_data() {
                                    companion = Some(font);
                                }
                                sub
                            }
                            None => font,
                        }
                    } else {
                        font
                    }
                }
                zpdf_font::EmbeddedOutlineProbe::PartiallyCorrupt
                | zpdf_font::EmbeddedOutlineProbe::Healthy => {
                    descriptor_spec =
                        Some(std::sync::Arc::new(zpdf_font::DescriptorSubstituteSpec {
                            lookup_name: (lookup_name != font.base_font)
                                .then(|| std::sync::Arc::<str>::from(lookup_name)),
                            hints,
                            ordering: ordering
                                .as_deref()
                                .and_then(zpdf_font::system::canonical_cid_ordering),
                            font_file_id: object_id,
                        }));
                    font
                }
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
    if let Some(ref mut emb) = companion {
        emb.cid_cmap = Some(cmap.clone());
        emb.dw2 = dw2;
        emb.validate_cid_cmap();
        if let Some(descriptor) = descriptor.as_ref() {
            apply_variations_from_descriptor(descriptor, emb);
        }
    }
    font.cid_cmap = Some(cmap);
    font.dw2 = dw2;
    // A Unicode-coded CMap is only usable when the font program can resolve
    // Unicode; otherwise fall back to Identity (codes pass through as CIDs).
    font.validate_cid_cmap();
    if let Some(descriptor) = descriptor.as_ref() {
        apply_variations_from_descriptor(descriptor, &mut font);
    }
    Ok((font, diagnostics, companion, descriptor_spec))
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
) -> Result<LoadedFontFallback> {
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
            match font.probe_embedded_outlines() {
                zpdf_font::EmbeddedOutlineProbe::Healthy
                | zpdf_font::EmbeddedOutlineProbe::PartiallyCorrupt => {
                    let lookup_name = substitute_font_name(&base_font, descriptor.as_ref());
                    let spec = std::sync::Arc::new(zpdf_font::DescriptorSubstituteSpec {
                        lookup_name: (lookup_name != font.base_font)
                            .then(|| std::sync::Arc::<str>::from(lookup_name)),
                        hints: descriptor_hints(file, descriptor.as_ref()),
                        ordering: None,
                        font_file_id: object_id,
                    });
                    Ok((
                        font,
                        zpdf_core::ProcessingDiagnostics::default(),
                        None,
                        Some(spec),
                    ))
                }
                zpdf_font::EmbeddedOutlineProbe::Unusable => {
                    tracing::warn!(
                        "font {base_font}: embedded program failed to parse; falling back to substitute"
                    );
                    let (fallback, companion) = match try_system_substitute_simple(
                        file,
                        descriptor.as_ref(),
                        &base_font,
                        font_type,
                        cid_widths.clone(),
                        provider,
                    ) {
                        Some(sub) => (sub, font.has_font_data().then_some(font)),
                        None if !font.has_font_data() => (
                            standard_simple_font(base_font, cid_widths).unwrap_or(font),
                            None,
                        ),
                        None => (font, None),
                    };
                    Ok((
                        fallback,
                        embedded_fallback_diagnostics(object_id, None),
                        companion,
                        None,
                    ))
                }
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
                None,
                None,
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
            None,
            None,
        )),
    }
}

fn type3_base_encoding(name: &str) -> Vec<String> {
    let Some(table) = zpdf_font::encoding::base_encoding_by_name(name) else {
        return Vec::new();
    };
    let mut encoding = vec![String::new(); 256];
    let mut last_non_empty = 0usize;
    for (code, slot) in encoding.iter_mut().enumerate() {
        if let Some(glyph_name) = table[code] {
            *slot = glyph_name.to_string();
            last_non_empty = code + 1;
        }
    }
    encoding.truncate(last_non_empty);
    encoding
}

fn apply_type3_differences(encoding: &mut Vec<String>, diffs: &[PdfObject]) {
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
}

#[cfg(test)]
fn type3_differences(diffs: &[PdfObject]) -> Vec<String> {
    let mut encoding = Vec::new();
    apply_type3_differences(&mut encoding, diffs);
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

    // FontBBox: declared glyph-space bounds (`[0 0 0 0]` means unspecified per
    // ISO 32000-1 Table 112).
    let type3_font_bbox = resolve_array(file, dict, "FontBBox").and_then(|arr| {
        let v: Vec<f64> = arr.iter().filter_map(|o| o.as_f64().ok()).collect();
        if v.len() == 4 && v.iter().all(|c| c.is_finite()) {
            let (x0, y0) = (v[0].min(v[2]), v[1].min(v[3]));
            let (x1, y1) = (v[0].max(v[2]), v[1].max(v[3]));
            (x1 > x0 && y1 > y0).then(|| zpdf_core::Rect::new(x0, y0, x1, y1))
        } else {
            None
        }
    });

    // Encoding: name or dict (/BaseEncoding + /Differences) → glyph name list.
    let mut encoding = Vec::new();
    if let Some(enc_obj) = dict.get("Encoding") {
        let resolved = match enc_obj {
            PdfObject::Ref(r) => file.resolve(*r).ok(),
            other => Some(other.clone()),
        };
        match resolved {
            Some(PdfObject::Name(name)) => {
                encoding = type3_base_encoding(name.as_str());
            }
            Some(PdfObject::Dict(enc_dict)) => {
                let base = match enc_dict.get("BaseEncoding") {
                    Some(PdfObject::Ref(r)) => file.resolve(*r).ok(),
                    other => other.cloned(),
                };
                if let Some(PdfObject::Name(base_name)) = base {
                    encoding = type3_base_encoding(base_name.as_str());
                }
                if let Some(diffs) = resolve_array(file, &enc_dict, "Differences") {
                    apply_type3_differences(&mut encoding, &diffs);
                }
            }
            _ => {}
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

    // Resolve the Type 3 font's own `/Resources` (XObjects, ColorSpaces,
    // ExtGStates) within the remaining font cache budget so CharProcs using
    // `/Name Do`, `/GS gs`, or named color spaces replay without a live PdfFile.
    let type3_resources = resolve_dict(file, dict, "Resources").and_then(|res_dict| {
        let mut seen_images = std::collections::HashMap::new();
        let mut form_cache = Type3FormCache::default();
        let mut active_forms = std::collections::HashSet::new();
        let (resolved, _) = resolve_type3_resources(
            file,
            &res_dict,
            &mut remaining,
            &mut seen_images,
            &mut form_cache,
            &mut active_forms,
            0,
        );
        (!resolved.is_empty()).then(|| Arc::new(resolved))
    });

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
        direct_composite_gids: false,
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
        type3_resources,
        type3_font_bbox,
    };

    Ok(font)
}

fn resolve_pdf_obj_shallow(file: &PdfFile, obj: &PdfObject, depth: u8) -> PdfObject {
    if depth >= 4 {
        return obj.clone();
    }
    match obj {
        PdfObject::Ref(id) => match file.resolve(*id) {
            Ok(resolved) => resolve_pdf_obj_shallow(file, &resolved, depth + 1),
            Err(_) => obj.clone(),
        },
        PdfObject::Array(arr) => PdfObject::Array(
            arr.iter()
                .map(|item| resolve_pdf_obj_shallow(file, item, depth + 1))
                .collect(),
        ),
        PdfObject::Dict(d) => {
            let mut out = zpdf_core::PdfDict::new();
            for (k, v) in &d.0 {
                out.insert(k.clone(), resolve_pdf_obj_shallow(file, v, depth + 1));
            }
            PdfObject::Dict(out)
        }
        other => other.clone(),
    }
}

fn resolve_type3_image_metadata(file: &PdfFile, dict: &zpdf_core::PdfDict) -> zpdf_core::PdfDict {
    let mut out = dict.clone();
    for key in [
        "Width",
        "Height",
        "BitsPerComponent",
        "ImageMask",
        "Decode",
        "ColorSpace",
        "Filter",
        "Intent",
        "Interpolate",
        "Matte",
    ] {
        if let Some(v) = dict.get(key) {
            let resolved = resolve_pdf_obj_shallow(file, v, 0);
            out.insert(zpdf_core::PdfName::new(key), resolved);
        }
    }
    if let Some(mask) = dict.get("Mask") {
        let resolved = match mask {
            PdfObject::Ref(id) => file.resolve(*id).ok(),
            other => Some(other.clone()),
        };
        if let Some(PdfObject::Array(arr)) = resolved {
            let inlined = arr
                .iter()
                .map(|o| resolve_pdf_obj_shallow(file, o, 0))
                .collect();
            out.insert(zpdf_core::PdfName::new("Mask"), PdfObject::Array(inlined));
        }
    }
    out
}

fn resolve_type3_image(
    file: &PdfFile,
    obj_id: ObjectId,
    stream: &zpdf_core::PdfStream,
    remaining: &mut u64,
    seen_images: &mut std::collections::HashMap<
        ObjectId,
        std::sync::Arc<zpdf_font::Type3ImageXObject>,
    >,
    depth: u8,
) -> Option<std::sync::Arc<zpdf_font::Type3ImageXObject>> {
    use std::sync::Arc;
    if depth > 4 {
        return None;
    }
    if let Some(cached) = seen_images.get(&obj_id) {
        return Some(Arc::clone(cached));
    }
    let data = match file.resolve_stream_data_with_limit(obj_id, *remaining) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("Type3 image XObject {obj_id}: skipped - {e}");
            return None;
        }
    };
    let after_data = remaining.checked_sub(data.len() as u64)?;
    let dict = resolve_type3_image_metadata(file, &stream.dict);
    let struct_bytes = std::mem::size_of::<zpdf_font::Type3ImageXObject>() as u64;
    let dict_bytes = zpdf_font::estimated_pdf_dict_bytes_within(&dict, after_data)?;
    *remaining = after_data.checked_sub(dict_bytes.checked_add(struct_bytes)?)?;

    let box_sub_image = |sub_id: ObjectId,
                         sub_stream: &zpdf_core::PdfStream,
                         remaining: &mut u64,
                         seen_images: &mut std::collections::HashMap<
        ObjectId,
        Arc<zpdf_font::Type3ImageXObject>,
    >|
     -> Option<Box<zpdf_font::Type3ImageXObject>> {
        let was_cached = seen_images.contains_key(&sub_id);
        let img = resolve_type3_image(file, sub_id, sub_stream, remaining, seen_images, depth + 1)?;
        if was_cached {
            // Boxing a previously cached image clones its owned dictionary.
            let extra = zpdf_font::estimated_pdf_dict_bytes_within(&img.dict, *remaining)?
                .checked_add(std::mem::size_of::<zpdf_font::Type3ImageXObject>() as u64)?;
            *remaining = remaining.checked_sub(extra)?;
        }
        Some(Box::new((*img).clone()))
    };

    let smask = stream.dict.get_ref("SMask").ok().and_then(|sm_id| {
        let sm_obj = file.resolve_shared(sm_id).ok()?;
        let sm_stream = sm_obj.as_stream().ok()?;
        box_sub_image(sm_id, sm_stream, remaining, seen_images)
    });
    let mask_stream = if smask.is_none() {
        stream.dict.get_ref("Mask").ok().and_then(|m_id| {
            let m_obj = file.resolve_shared(m_id).ok()?;
            let m_stream = m_obj.as_stream().ok()?;
            box_sub_image(m_id, m_stream, remaining, seen_images)
        })
    } else {
        None
    };

    let resolved = Arc::new(zpdf_font::Type3ImageXObject {
        dict,
        data: Arc::from(data),
        smask,
        mask_stream,
    });
    seen_images.insert(obj_id, Arc::clone(&resolved));
    Some(resolved)
}

fn type3_map_entry_bytes<V>(name: &str) -> u64 {
    (name.len() as u64).saturating_add(
        ((std::mem::size_of::<(String, V)>() as u64).saturating_add(1)).saturating_mul(2),
    )
}

#[derive(Default)]
struct Type3FormCache {
    /// Decoded Form stream bytes shared across depth-limited and complete
    /// resolutions of the same Form object ID so stream bytes are charged once.
    streams: std::collections::HashMap<ObjectId, std::sync::Arc<[u8]>>,
    /// Forms whose entire resource subtree resolved without hitting the depth
    /// cap or an active ancestor cycle; safe to reuse at any depth.
    complete: std::collections::HashMap<ObjectId, std::sync::Arc<zpdf_font::Type3FormXObject>>,
    /// Forms whose subtree hit the depth cap (without hitting an ancestor
    /// cycle), keyed by `(ObjectId, depth)` so DAGs at the same depth share one
    /// truncated Form without poisoning shallower aliases.
    depth_limited:
        std::collections::HashMap<(ObjectId, u8), std::sync::Arc<zpdf_font::Type3FormXObject>>,
    /// Pre-resolved `/Shading` entries deduplicated by indirect object ID.
    shadings: std::collections::HashMap<ObjectId, std::sync::Arc<zpdf_font::Type3Shading>>,
    /// Pre-resolved PDF functions (shading `/Function` and `/SMask /TR`,
    /// including stitching sub-functions) deduplicated by indirect object ID,
    /// paired with their subtree height so cache hits enforce the same maximum
    /// nesting depth as cold lookups.
    functions: std::collections::HashMap<ObjectId, (std::sync::Arc<zpdf_font::Type3Function>, u8)>,
}

const MAX_TYPE3_FUNCTION_DEPTH: u8 = 8;

fn resolve_type3_function_inner(
    file: &PdfFile,
    obj: &PdfObject,
    remaining: &mut u64,
    form_cache: &mut Type3FormCache,
    newly_cached: &mut Vec<ObjectId>,
    depth: u8,
) -> Option<(std::sync::Arc<zpdf_font::Type3Function>, u8)> {
    use std::sync::Arc;
    if depth > MAX_TYPE3_FUNCTION_DEPTH {
        return None;
    }
    let obj_id = match obj {
        PdfObject::Ref(id) => {
            if let Some((cached, height)) = form_cache.functions.get(id) {
                if depth.saturating_add(*height) > MAX_TYPE3_FUNCTION_DEPTH {
                    return None;
                }
                return Some((Arc::clone(cached), *height));
            }
            Some(*id)
        }
        _ => None,
    };

    let struct_cost = std::mem::size_of::<zpdf_font::Type3Function>() as u64;
    let after_struct = remaining.checked_sub(struct_cost)?;
    *remaining = after_struct;

    let (resolved_dict, stream_bytes) = match obj {
        PdfObject::Ref(id) => {
            let shared = file.resolve_shared(*id).ok()?;
            match shared.as_ref() {
                PdfObject::Stream(s) => {
                    let raw = file.resolve_stream_data_with_limit(*id, *remaining).ok()?;
                    let data_len = raw.len() as u64;
                    let next = remaining.checked_sub(data_len)?;
                    *remaining = next;
                    (s.dict.clone(), Some(Arc::<[u8]>::from(raw)))
                }
                PdfObject::Dict(d) => (d.clone(), None),
                _ => return None,
            }
        }
        PdfObject::Dict(d) => (d.clone(), None),
        _ => return None,
    };

    let mut clean_dict = zpdf_core::PdfDict::new();
    for (k, v) in &resolved_dict.0 {
        if k.as_str() == "Functions" {
            continue;
        }
        clean_dict.insert(k.clone(), resolve_pdf_obj_shallow(file, v, 0));
    }
    let dict_cost = zpdf_font::estimated_pdf_dict_bytes_within(&clean_dict, *remaining)?;
    *remaining -= dict_cost;

    let (func, height) = if let Some(data) = stream_bytes {
        (
            Arc::new(zpdf_font::Type3Function::Stream {
                dict: clean_dict,
                data,
            }),
            0u8,
        )
    } else {
        let mut children = Vec::new();
        let mut max_child_height = 0u8;
        if let Some(sub_funcs) = resolved_dict.get("Functions") {
            let sub_resolved = match sub_funcs {
                PdfObject::Ref(id) => file.resolve(*id).ok(),
                other => Some(other.clone()),
            };
            if let Some(PdfObject::Array(arr)) = sub_resolved {
                for item in &arr {
                    let (child, child_height) = resolve_type3_function_inner(
                        file,
                        item,
                        remaining,
                        form_cache,
                        newly_cached,
                        depth + 1,
                    )?;
                    max_child_height = max_child_height.max(child_height);
                    children.push(child);
                }
                let vec_cost = (children.capacity() as u64)
                    .saturating_mul(std::mem::size_of::<Arc<zpdf_font::Type3Function>>() as u64);
                let next = remaining.checked_sub(vec_cost)?;
                *remaining = next;
            }
        }
        let height = if children.is_empty() {
            0
        } else {
            max_child_height.saturating_add(1)
        };
        (
            Arc::new(zpdf_font::Type3Function::Dict {
                dict: clean_dict,
                children,
            }),
            height,
        )
    };

    if let Some(id) = obj_id {
        form_cache.functions.insert(id, (Arc::clone(&func), height));
        newly_cached.push(id);
    }
    Some((func, height))
}

fn resolve_type3_shading(
    file: &PdfFile,
    obj: &PdfObject,
    remaining: &mut u64,
    form_cache: &mut Type3FormCache,
) -> Option<std::sync::Arc<zpdf_font::Type3Shading>> {
    use std::sync::Arc;
    let obj_id = match obj {
        PdfObject::Ref(id) => {
            if let Some(cached) = form_cache.shadings.get(id) {
                return Some(Arc::clone(cached));
            }
            Some(*id)
        }
        _ => None,
    };

    let saved_rem = *remaining;
    let mut newly_cached_funcs = Vec::new();
    let rollback = |remaining: &mut u64,
                    form_cache: &mut Type3FormCache,
                    newly_cached_funcs: Vec<ObjectId>| {
        *remaining = saved_rem;
        for id in newly_cached_funcs {
            form_cache.functions.remove(&id);
        }
    };
    let struct_cost = std::mem::size_of::<zpdf_font::Type3Shading>() as u64;
    let after_struct = remaining.checked_sub(struct_cost)?;
    *remaining = after_struct;

    let (raw_dict, stream_data) = match obj {
        PdfObject::Ref(id) => {
            let Ok(shared) = file.resolve_shared(*id) else {
                rollback(remaining, form_cache, newly_cached_funcs);
                return None;
            };
            match shared.as_ref() {
                PdfObject::Stream(s) => {
                    let Ok(raw) = file.resolve_stream_data_with_limit(*id, *remaining) else {
                        rollback(remaining, form_cache, newly_cached_funcs);
                        return None;
                    };
                    let data_len = raw.len() as u64;
                    let Some(next) = remaining.checked_sub(data_len) else {
                        rollback(remaining, form_cache, newly_cached_funcs);
                        return None;
                    };
                    *remaining = next;
                    (s.dict.clone(), Some(Arc::<[u8]>::from(raw)))
                }
                PdfObject::Dict(d) => (d.clone(), None),
                _ => {
                    rollback(remaining, form_cache, newly_cached_funcs);
                    return None;
                }
            }
        }
        PdfObject::Dict(d) => (d.clone(), None),
        _ => {
            rollback(remaining, form_cache, newly_cached_funcs);
            return None;
        }
    };

    let mut clean_dict = zpdf_core::PdfDict::new();
    for key in [
        "ShadingType",
        "ColorSpace",
        "Background",
        "BBox",
        "Coords",
        "Domain",
        "Extend",
        "BitsPerCoordinate",
        "BitsPerComponent",
        "BitsPerFlag",
        "VerticesPerRow",
        "Decode",
    ] {
        if let Some(v) = raw_dict.get(key) {
            clean_dict.insert(
                zpdf_core::PdfName::new(key),
                resolve_pdf_obj_shallow(file, v, 0),
            );
        }
    }
    let Some(dict_cost) = zpdf_font::estimated_pdf_dict_bytes_within(&clean_dict, *remaining)
    else {
        rollback(remaining, form_cache, newly_cached_funcs);
        return None;
    };
    *remaining -= dict_cost;

    let mut functions = Vec::new();
    if let Some(func_obj) = raw_dict.get("Function") {
        let resolved_func_obj = match func_obj {
            PdfObject::Ref(id) => match file.resolve(*id).ok() {
                Some(PdfObject::Array(arr)) => Some(PdfObject::Array(arr)),
                _ => Some(func_obj.clone()),
            },
            other => Some(other.clone()),
        };
        if let Some(PdfObject::Array(arr)) = resolved_func_obj {
            for item in &arr {
                let Some((f, _)) = resolve_type3_function_inner(
                    file,
                    item,
                    remaining,
                    form_cache,
                    &mut newly_cached_funcs,
                    0,
                ) else {
                    rollback(remaining, form_cache, newly_cached_funcs);
                    return None;
                };
                functions.push(f);
            }
        } else if let Some(item) = resolved_func_obj {
            let Some((f, _)) = resolve_type3_function_inner(
                file,
                &item,
                remaining,
                form_cache,
                &mut newly_cached_funcs,
                0,
            ) else {
                rollback(remaining, form_cache, newly_cached_funcs);
                return None;
            };
            functions.push(f);
        }
        let vec_cost = (functions.capacity() as u64)
            .saturating_mul(std::mem::size_of::<Arc<zpdf_font::Type3Function>>() as u64);
        let Some(next) = remaining.checked_sub(vec_cost) else {
            rollback(remaining, form_cache, newly_cached_funcs);
            return None;
        };
        *remaining = next;
    }

    let shading = Arc::new(zpdf_font::Type3Shading {
        dict: clean_dict,
        stream_data,
        functions,
    });
    if let Some(id) = obj_id {
        form_cache.shadings.insert(id, Arc::clone(&shading));
    }
    Some(shading)
}

#[derive(Default, Clone, Copy)]
struct Type3ResourceLimits {
    hit_depth_limit: bool,
    hit_cycle: bool,
}

#[allow(clippy::too_many_arguments)]
fn resolve_type3_form(
    file: &PdfFile,
    id: ObjectId,
    stream: &zpdf_core::PdfStream,
    remaining: &mut u64,
    seen_images: &mut std::collections::HashMap<
        ObjectId,
        std::sync::Arc<zpdf_font::Type3ImageXObject>,
    >,
    form_cache: &mut Type3FormCache,
    active_forms: &mut std::collections::HashSet<ObjectId>,
    depth: u8,
) -> (
    Option<std::sync::Arc<zpdf_font::Type3FormXObject>>,
    Type3ResourceLimits,
) {
    use std::sync::Arc;
    if let Some(cached) = form_cache.complete.get(&id) {
        return (Some(Arc::clone(cached)), Type3ResourceLimits::default());
    }
    if let Some(cached) = form_cache.depth_limited.get(&(id, depth)) {
        return (
            Some(Arc::clone(cached)),
            Type3ResourceLimits {
                hit_depth_limit: true,
                hit_cycle: false,
            },
        );
    }
    if !active_forms.insert(id) {
        return (
            None,
            Type3ResourceLimits {
                hit_depth_limit: false,
                hit_cycle: true,
            },
        );
    }
    let saved_rem = *remaining;
    let decoded_stream: Option<(Arc<[u8]>, u64)> =
        if let Some(cached_data) = form_cache.streams.get(&id) {
            Some((Arc::clone(cached_data), 0))
        } else {
            file.resolve_stream_data_with_limit(id, *remaining)
                .ok()
                .map(|raw| {
                    let len = raw.len() as u64;
                    (Arc::from(raw), len)
                })
        };
    let mut form_limits = Type3ResourceLimits::default();
    let mut out_form = None;
    if let Some((data, data_charge)) = decoded_stream {
        let mut form_dict = zpdf_core::PdfDict::new();
        for key in ["BBox", "Matrix", "Group"] {
            if let Some(v) = stream.dict.get(key) {
                form_dict.insert(
                    zpdf_core::PdfName::new(key),
                    resolve_pdf_obj_shallow(file, v, 0),
                );
            }
        }
        let struct_bytes = std::mem::size_of::<zpdf_font::Type3FormXObject>() as u64;
        let form_cost = remaining
            .checked_sub(data_charge)
            .and_then(|r| r.checked_sub(struct_bytes))
            .and_then(|r| {
                zpdf_font::estimated_pdf_dict_bytes_within(&form_dict, r)
                    .and_then(|dict_bytes| r.checked_sub(dict_bytes))
            });
        if let Some(next_rem) = form_cost {
            *remaining = next_rem;
            form_cache
                .streams
                .entry(id)
                .or_insert_with(|| Arc::clone(&data));
            let form_res = resolve_dict(file, &stream.dict, "Resources").and_then(|rd| {
                let (r, sub_limits) = resolve_type3_resources(
                    file,
                    &rd,
                    remaining,
                    seen_images,
                    form_cache,
                    active_forms,
                    depth + 1,
                );
                form_limits = sub_limits;
                (!r.is_empty()).then(|| Arc::new(r))
            });
            let form = Arc::new(zpdf_font::Type3FormXObject {
                dict: form_dict,
                data,
                resources: form_res,
            });
            if !form_limits.hit_cycle {
                if form_limits.hit_depth_limit {
                    form_cache
                        .depth_limited
                        .insert((id, depth), Arc::clone(&form));
                } else {
                    form_cache.complete.insert(id, Arc::clone(&form));
                }
            }
            out_form = Some(form);
        } else {
            *remaining = saved_rem;
        }
    }
    active_forms.remove(&id);
    (out_form, form_limits)
}

fn resolve_type3_resources(
    file: &PdfFile,
    res_dict: &zpdf_core::PdfDict,
    remaining: &mut u64,
    seen_images: &mut std::collections::HashMap<
        ObjectId,
        std::sync::Arc<zpdf_font::Type3ImageXObject>,
    >,
    form_cache: &mut Type3FormCache,
    active_forms: &mut std::collections::HashSet<ObjectId>,
    depth: u8,
) -> (zpdf_font::Type3Resources, Type3ResourceLimits) {
    if depth > 8 {
        return (
            zpdf_font::Type3Resources::default(),
            Type3ResourceLimits {
                hit_depth_limit: true,
                hit_cycle: false,
            },
        );
    }
    if *remaining == 0 {
        return (
            zpdf_font::Type3Resources::default(),
            Type3ResourceLimits::default(),
        );
    }
    let mut out = zpdf_font::Type3Resources::default();
    let mut limits = Type3ResourceLimits::default();

    if let Some(xobj_dict) = resolve_dict(file, res_dict, "XObject") {
        for (name, obj) in &xobj_dict.0 {
            let entry_cost = type3_map_entry_bytes::<zpdf_font::Type3XObject>(&name.0);
            if *remaining < entry_cost {
                continue;
            }
            let PdfObject::Ref(id) = obj else {
                continue;
            };
            let Ok(resolved_obj) = file.resolve_shared(*id) else {
                continue;
            };
            let Ok(stream) = resolved_obj.as_stream() else {
                continue;
            };
            match stream.dict.get_name("Subtype").unwrap_or_default() {
                "Image" => {
                    let saved_rem = *remaining;
                    *remaining -= entry_cost;
                    if let Some(img) =
                        resolve_type3_image(file, *id, stream, remaining, seen_images, 0)
                    {
                        out.xobjects
                            .insert(name.0.clone(), zpdf_font::Type3XObject::Image(img));
                    } else {
                        *remaining = saved_rem;
                    }
                }
                "Form" => {
                    let saved_rem = *remaining;
                    *remaining -= entry_cost;
                    let (form, form_limits) = resolve_type3_form(
                        file,
                        *id,
                        stream,
                        remaining,
                        seen_images,
                        form_cache,
                        active_forms,
                        depth,
                    );
                    limits.hit_depth_limit |= form_limits.hit_depth_limit;
                    limits.hit_cycle |= form_limits.hit_cycle;
                    if let Some(form) = form {
                        out.xobjects
                            .insert(name.0.clone(), zpdf_font::Type3XObject::Form(form));
                    } else {
                        *remaining = saved_rem;
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(cs_dict) = resolve_dict(file, res_dict, "ColorSpace") {
        for (name, obj) in &cs_dict.0 {
            let entry_cost = type3_map_entry_bytes::<PdfObject>(&name.0);
            let Some(after_entry) = remaining.checked_sub(entry_cost) else {
                continue;
            };
            let resolved = resolve_pdf_obj_shallow(file, obj, 0);
            let Some(val_cost) =
                zpdf_font::estimated_pdf_object_bytes_within(&resolved, after_entry)
            else {
                continue;
            };
            *remaining = after_entry - val_cost;
            out.color_spaces.insert(name.0.clone(), resolved);
        }
    }

    if let Some(gs_dict) = resolve_dict(file, res_dict, "ExtGState") {
        for (name, obj) in &gs_dict.0 {
            let resolved = match obj {
                PdfObject::Ref(id) => file.resolve(*id).ok(),
                other => Some(other.clone()),
            };
            if let Some(PdfObject::Dict(d)) = resolved {
                let entry_cost = type3_map_entry_bytes::<zpdf_core::PdfDict>(&name.0);
                if let Some(after_entry) = remaining.checked_sub(entry_cost) {
                    let mut inlined = zpdf_core::PdfDict::new();
                    for key in [
                        "ca", "CA", "BM", "LW", "LC", "LJ", "ML", "D", "OP", "op", "OPM", "RI",
                    ] {
                        if let Some(v) = d.get(key) {
                            inlined.insert(
                                zpdf_core::PdfName::new(key),
                                resolve_pdf_obj_shallow(file, v, 0),
                            );
                        }
                    }
                    if let Some(val_cost) =
                        zpdf_font::estimated_pdf_dict_bytes_within(&inlined, after_entry)
                    {
                        *remaining = after_entry - val_cost;
                        out.ext_g_states.insert(name.0.clone(), inlined);
                    }
                }

                // Retain `/SMask` (`/None` or soft-mask dict with replayable `/G`
                // transparency group Form XObject and optional `/BC` & `/TR`)
                // within the font cache budget for standalone Type 3 replay.
                if let Some(sm_raw) = d.get("SMask") {
                    let sm_resolved = match sm_raw {
                        PdfObject::Ref(id) => file.resolve(*id).ok(),
                        other => Some(other.clone()),
                    };
                    let sm_entry_cost = type3_map_entry_bytes::<zpdf_font::Type3SoftMask>(&name.0);
                    match sm_resolved {
                        Some(PdfObject::Name(n)) if n.as_str() == "None" => {
                            if let Some(next_rem) = remaining.checked_sub(sm_entry_cost) {
                                *remaining = next_rem;
                                out.ext_g_state_smasks
                                    .insert(name.0.clone(), zpdf_font::Type3SoftMask::None);
                            }
                        }
                        Some(PdfObject::Dict(sm_dict)) => {
                            if let Ok(g_id) = sm_dict.get_ref("G") {
                                if let Ok(g_obj) = file.resolve_shared(g_id) {
                                    if let Ok(g_stream) = g_obj.as_stream() {
                                        let saved_rem = *remaining;
                                        if let Some(after_entry) =
                                            remaining.checked_sub(sm_entry_cost)
                                        {
                                            *remaining = after_entry;
                                            let is_alpha =
                                                matches!(sm_dict.get_name("S"), Ok("Alpha"));
                                            let backdrop_luma = resolve_array(file, &sm_dict, "BC")
                                                .map(|arr| {
                                                    let vals: Vec<f64> = arr
                                                        .iter()
                                                        .filter_map(|o| o.as_f64().ok())
                                                        .collect();
                                                    if vals.is_empty() {
                                                        0.0
                                                    } else {
                                                        (vals.iter().sum::<f64>()
                                                            / vals.len() as f64)
                                                            .clamp(0.0, 1.0)
                                                            as f32
                                                    }
                                                })
                                                .unwrap_or(0.0);
                                            let mut newly_cached_tr_funcs = Vec::new();
                                            let transfer = sm_dict.get("TR").and_then(|tr_obj| {
                                                let is_identity = match tr_obj {
                                                    PdfObject::Ref(tr_id) => matches!(
                                                        file.resolve(*tr_id).ok(),
                                                        Some(PdfObject::Name(n))
                                                            if n.as_str() == "Identity"
                                                                || n.as_str() == "Default"
                                                    ),
                                                    PdfObject::Name(n) => {
                                                        n.as_str() == "Identity"
                                                            || n.as_str() == "Default"
                                                    }
                                                    _ => false,
                                                };
                                                if is_identity {
                                                    return None;
                                                }
                                                let tr_saved_rem = *remaining;
                                                match resolve_type3_function_inner(
                                                    file,
                                                    tr_obj,
                                                    remaining,
                                                    form_cache,
                                                    &mut newly_cached_tr_funcs,
                                                    0,
                                                ) {
                                                    Some((func, _)) => Some(func),
                                                    None => {
                                                        *remaining = tr_saved_rem;
                                                        for id in newly_cached_tr_funcs.drain(..) {
                                                            form_cache.functions.remove(&id);
                                                        }
                                                        None
                                                    }
                                                }
                                            });
                                            let (group, form_limits) = resolve_type3_form(
                                                file,
                                                g_id,
                                                g_stream,
                                                remaining,
                                                seen_images,
                                                form_cache,
                                                active_forms,
                                                depth,
                                            );
                                            limits.hit_depth_limit |= form_limits.hit_depth_limit;
                                            limits.hit_cycle |= form_limits.hit_cycle;
                                            if let Some(group) = group {
                                                out.ext_g_state_smasks.insert(
                                                    name.0.clone(),
                                                    zpdf_font::Type3SoftMask::Dict {
                                                        is_alpha,
                                                        group,
                                                        backdrop_luma,
                                                        transfer,
                                                    },
                                                );
                                            } else {
                                                *remaining = saved_rem;
                                                for id in newly_cached_tr_funcs {
                                                    form_cache.functions.remove(&id);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if let Some(sh_dict) = resolve_dict(file, res_dict, "Shading") {
        for (name, obj) in &sh_dict.0 {
            let entry_cost =
                type3_map_entry_bytes::<std::sync::Arc<zpdf_font::Type3Shading>>(&name.0);
            let saved_rem = *remaining;
            let Some(after_entry) = remaining.checked_sub(entry_cost) else {
                continue;
            };
            *remaining = after_entry;
            if let Some(shading) = resolve_type3_shading(file, obj, remaining, form_cache) {
                out.shadings.insert(name.0.clone(), shading);
            } else {
                *remaining = saved_rem;
            }
        }
    }

    (out, limits)
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
    extract_font_file_with_limit(file, fd_dict, None)
}

fn extract_font_file_with_limit(
    file: &PdfFile,
    fd_dict: &zpdf_core::PdfDict,
    max_decoded_bytes: Option<u64>,
) -> EmbeddedFontFile {
    let mut first_failure: Option<(Option<ObjectId>, Option<zpdf_core::Error>)> = None;
    // Try FontFile2 (TrueType), then FontFile3 (OpenType/CFF), then FontFile (Type1)
    for key in &["FontFile2", "FontFile3", "FontFile"] {
        match fd_dict.get(key) {
            None | Some(PdfObject::Null) => continue,
            Some(PdfObject::Ref(ff_ref)) => {
                let decoded = match max_decoded_bytes {
                    Some(limit) => file.resolve_stream_data_with_limit(*ff_ref, limit),
                    None => file.resolve_stream_data(*ff_ref),
                };
                match decoded {
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
                }
            }
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
        apply_differences(None, &dict, &mut encoding);
    }

    #[test]
    fn indirect_differences_updates_extraction_encoding() {
        let file = PdfFile::parse(crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>",
            "<< /Type /Font /Subtype /Type3 /FontMatrix [0.001 0 0 0.001 0 0] \
             /FontBBox [0 0 1000 1000] /CharProcs << /B 5 0 R >> \
             /Encoding 3 0 R /FirstChar 65 /LastChar 65 /Widths [600] >>",
            "<< /Type /Encoding /BaseEncoding /WinAnsiEncoding /Differences 4 0 R >>",
            "[65 /B]",
            "<< /Length 2 >>\nstream\nd0\nendstream",
        ]))
        .expect("parse");
        let font = load_single_font_with_provider(&file, ObjectId(2, 0), None).expect("font");
        let mut decoded = String::new();
        font.append_decoded_source_code(65, 1, &mut decoded);
        assert_eq!(
            decoded, "B",
            "indirect /Differences array must remap extraction encoding from WinAnsi /A to /B"
        );
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

    #[test]
    fn type3_indirect_base_encoding_is_resolved() {
        let file = PdfFile::parse(crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>",
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
               /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /A 4 0 R >> \
               /Encoding << /Type /Encoding /BaseEncoding 3 0 R >> \
               /FirstChar 65 /LastChar 65 /Widths [1000] >>",
            "/WinAnsiEncoding",
            "<< /Length 26 >>\nstream\n1000 0 d0 0 0 100 100 re f\nendstream",
        ]))
        .expect("parse");

        let font_dict = file.resolve(ObjectId(2, 0)).unwrap();
        let font = load_single_font_dict(&file, font_dict.as_dict().unwrap()).expect("load type3");
        let (glyph_name, bytes, _) = font
            .type3_glyph_entry(65)
            .expect("code 65 must resolve via indirect /BaseEncoding /WinAnsiEncoding");
        assert_eq!(glyph_name, "A");
        assert!(!bytes.is_empty());
        let mut decoded = String::new();
        assert!(font.append_verified_type3_unicode(65, 1, &mut decoded));
        assert_eq!(decoded, "A");
    }

    #[test]
    fn type3_loader_charges_retained_pdf_metadata_against_budget() {
        // Type 3 font whose /Resources holds an /Indexed ColorSpace with a 4 KiB
        // lookup string and an /ExtGState with a 256-entry dash array, but tiny
        // stream bytes. Under a 1 KiB font cache budget, the loader must reject
        // the oversized metadata instead of letting it bypass budget checks.
        let big_lookup = "A".repeat(4096);
        let big_dash = "1 ".repeat(256);
        let font_obj = format!(
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] \
               /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 3 0 R >> \
               /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [10] \
               /Resources << /ColorSpace << /CSBig [/Indexed /DeviceRGB 255 ({big_lookup})] >> \
                             /ExtGState << /GSBig << /D [[{big_dash}] 0] >> >> >> >>"
        );
        let pdf = crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>",
            &font_obj,
            "<< /Length 6 >>\nstream\n1 0 d0\nendstream",
        ]);

        let small_limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: 1024,
            ..Default::default()
        };
        let file = PdfFile::parse_with_limits(pdf.clone(), small_limits).expect("parse");
        let font = load_single_font(&file, ObjectId(2, 0)).expect("load type3");
        assert!(
            font.type3_resources().is_none(),
            "oversized ColorSpace/ExtGState metadata must be omitted under 1 KiB loader budget"
        );
        let base_est = font.estimated_cache_bytes();

        // With a generous budget, both ColorSpace and ExtGState metadata load
        // and are reflected in `estimated_cache_bytes()`.
        let large_limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: 64 * 1024,
            ..Default::default()
        };
        let file_big = PdfFile::parse_with_limits(pdf, large_limits).expect("parse");
        let font_big = load_single_font(&file_big, ObjectId(2, 0)).expect("load type3");
        let res = font_big
            .type3_resources()
            .expect("metadata must load within 64 KiB budget");
        assert!(res.color_spaces.contains_key("CSBig"));
        assert!(res.ext_g_states.contains_key("GSBig"));
        let res_est = res.estimated_cache_bytes();
        assert!(
            res_est > 10_000,
            "loaded metadata must be accounted for in Type3Resources::estimated_cache_bytes(), got {res_est}"
        );
        assert!(
            font_big.estimated_cache_bytes() >= base_est + res_est,
            "font estimated_cache_bytes ({}) must include Type3Resources metadata ({res_est} over base {base_est})",
            font_big.estimated_cache_bytes()
        );
    }

    #[test]
    fn type3_deep_form_alias_does_not_poison_complete_shallow_alias() {
        // Object layout:
        // 1: Catalog
        // 2: Type 3 font whose /Resources visits /DeepChain (obj 4) BEFORE /Shallow (obj 12)
        // 3: CharProc drawing /Shallow Do
        // 4..11: 8-deep Form chain (depths 0..7) leading to shared Form 12 at depth 8
        // 12: Shared Form whose /Resources defines /LeafImg (obj 13)
        // 13: 2x2 opaque red RGB image
        // When visited via /DeepChain at depth 8, Form 12's child /Resources
        // lookup hits depth 9 (> 8) and returns empty. The subsequent direct
        // /Shallow reference at depth 0 MUST NOT reuse that depth-truncated Form
        // and MUST retain /LeafImg in its resources.
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog >>".into(), // 1
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] \
               /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 3 0 R >> \
               /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [10] \
               /Resources << /XObject << /DeepChain 4 0 R /Shallow 12 0 R >> >> >>"
                .into(), // 2
            "<< /Length 19 >>\nstream\n10 0 d0 /Shallow Do\nendstream".into(), // 3
        ];
        // Objects 4..=11 chain to the next Form (4->5->...->11->12).
        for obj_num in 4..=11u32 {
            let next = obj_num + 1;
            objs.push(format!(
                "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] \
                   /Resources << /XObject << /Next {next} 0 R >> >> /Length 8 >>\n\
                 stream\n/Next Do\nendstream"
            ));
        }
        // Object 12: shared Form defining /LeafImg in its own /Resources.
        objs.push(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] \
               /Resources << /XObject << /LeafImg 13 0 R >> >> /Length 27 >>\n\
             stream\n10 0 0 10 0 0 cm /LeafImg Do\nendstream"
                .into(),
        );
        // Object 13: 2x2 red DeviceRGB image (12 raw bytes).
        objs.push(
            "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 \
               /BitsPerComponent 8 /ColorSpace /DeviceRGB /Length 12 >>\n\
             stream\n\x7f\x00\x00\x7f\x00\x00\x7f\x00\x00\x7f\x00\x00\nendstream"
                .into(),
        );
        let obj_refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        let pdf = crate::test_util::build_pdf(&obj_refs);
        let file = PdfFile::parse(pdf).expect("parse");
        let font = load_single_font(&file, ObjectId(2, 0)).expect("load type3");
        let res = font
            .type3_resources()
            .expect("type3 font must retain resolved /Resources");

        // The deep alias at depth 8 was depth-truncated (`resources: None`).
        let mut cursor = res.xobjects.get("DeepChain");
        for step in 0..8 {
            let Some(zpdf_font::Type3XObject::Form(f)) = cursor else {
                panic!("expected Form at deep chain step {step}");
            };
            cursor = f.resources.as_ref().and_then(|r| r.xobjects.get("Next"));
        }
        let Some(zpdf_font::Type3XObject::Form(deep_leaf)) = cursor else {
            panic!("expected shared Form at depth 8 of DeepChain");
        };
        assert!(
            deep_leaf.resources.is_none(),
            "depth-8 leaf should have been truncated by depth limit"
        );

        // The shallow alias at depth 0 MUST resolve completely with /LeafImg!
        let Some(zpdf_font::Type3XObject::Form(shallow)) = res.xobjects.get("Shallow") else {
            panic!("expected /Shallow Form XObject");
        };
        let shallow_res = shallow
            .resources
            .as_ref()
            .expect("/Shallow Form must retain its /Resources after deep alias was visited first");
        assert!(
            matches!(
                shallow_res.xobjects.get("LeafImg"),
                Some(zpdf_font::Type3XObject::Image(_))
            ),
            "/Shallow Form must retain /LeafImg Image XObject"
        );
        // Both aliases share the same decoded Form stream buffer Arc.
        assert!(
            std::sync::Arc::ptr_eq(&deep_leaf.data, &shallow.data),
            "depth-limited and complete aliases should share decoded stream bytes"
        );
    }

    #[test]
    fn type3_loader_retains_ext_gstate_smask_within_budget() {
        // Type 3 font whose /ExtGState carries `/GSMask` (a luminosity soft mask
        // whose /G transparency group draws a left-half white rect) and `/GSClear`
        // (`/SMask /None`). Must load and account for the mask group within a
        // generous budget and decline the oversized mask group under a tight budget.
        let pdf = crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>", // 1
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] \
               /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 3 0 R >> \
               /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [10] \
               /Resources << /ExtGState << /GSMask 4 0 R /GSClear << /SMask /None >> >> >> >>", // 2
            "<< /Length 36 >>\nstream\n10 0 d0 /GSMask gs 1 0 0 rg 0 0 10 10 re f\nendstream", // 3
            "<< /Type /ExtGState /SMask << /S /Luminosity /G 5 0 R >> >>", // 4
            "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] \
               /Group << /Type /Group /S /Transparency /CS /DeviceGray >> \
               /Resources << /XObject << /Sub 6 0 R >> >> /Length 7 >>\n\
             stream\n/Sub Do\nendstream", // 5
            "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] /Length 19 >>\n\
             stream\n1 g 0 0 5 10 re f\nendstream", // 6
        ]);

        let file = PdfFile::parse(pdf.clone()).expect("parse");
        let font = load_single_font(&file, ObjectId(2, 0)).expect("load type3");
        let res = font
            .type3_resources()
            .expect("Type 3 font must retain /Resources with /ExtGState /SMask");
        assert!(
            matches!(
                res.ext_g_state_smasks.get("GSClear"),
                Some(zpdf_font::Type3SoftMask::None)
            ),
            "expected /GSClear to retain Type3SoftMask::None"
        );
        let Some(zpdf_font::Type3SoftMask::Dict {
            is_alpha, group, ..
        }) = res.ext_g_state_smasks.get("GSMask")
        else {
            panic!("expected /GSMask to retain Type3SoftMask::Dict");
        };
        assert!(!*is_alpha);
        assert!(
            group
                .resources
                .as_ref()
                .is_some_and(|r| r.xobjects.contains_key("Sub")),
            "/SMask /G must retain its own nested /Resources"
        );
        let full_est = res.estimated_cache_bytes();
        assert!(full_est > 0);

        // Under a tiny budget that cannot fit the /G group stream + structs,
        // the loader must omit /GSMask without exceeding the budget.
        let tiny_limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: 96,
            ..Default::default()
        };
        let tiny_file = PdfFile::parse_with_limits(pdf, tiny_limits).expect("parse tiny");
        let tiny_font = load_single_font(&tiny_file, ObjectId(2, 0)).expect("load tiny");
        assert!(
            tiny_font
                .type3_resources()
                .is_none_or(|r| !r.ext_g_state_smasks.contains_key("GSMask")),
            "oversized /SMask group must be rejected under tight font budget"
        );
    }

    #[test]
    fn type3_loader_retains_shading_resources_within_budget() {
        let pdf = crate::test_util::build_pdf(&[
            "<< /Type /Catalog >>", // 1
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 80 20] \
               /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 3 0 R >> \
               /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [80] \
               /Resources << /ExtGState << /GSFade 4 0 R >> >> >>", // 2
            "<< /Length 40 >>\nstream\n80 0 d0 /GSFade gs 1 0 0 rg 0 0 80 20 re f\nendstream", // 3
            "<< /Type /ExtGState /SMask << /S /Luminosity /G 5 0 R >> >>", // 4
            "<< /Type /XObject /Subtype /Form /BBox [0 0 80 20] \
               /Group << /Type /Group /S /Transparency /CS /DeviceGray >> \
               /Resources << /Shading << /Fade 6 0 R >> >> /Length 8 >>\n\
             stream\n/Fade sh\nendstream", // 5
            "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 80 0] \
               /Function 7 0 R /Extend [true true] >>", // 6
            "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>", // 7
        ]);

        let file = PdfFile::parse(pdf.clone()).expect("parse");
        let font = load_single_font(&file, ObjectId(2, 0)).expect("load type3");
        let res = font
            .type3_resources()
            .expect("Type 3 font must retain /Resources");
        let Some(zpdf_font::Type3SoftMask::Dict { group, .. }) =
            res.ext_g_state_smasks.get("GSFade")
        else {
            panic!("expected /GSFade soft mask");
        };
        let shading = group
            .resources
            .as_ref()
            .and_then(|r| r.shadings.get("Fade"))
            .expect("/SMask /G Form must retain /Resources /Shading /Fade");
        assert_eq!(shading.dict.get_i64("ShadingType").ok(), Some(2));
        assert_eq!(shading.functions.len(), 1);
        assert!(res.estimated_cache_bytes() > 0);

        let tiny_limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: 96,
            ..Default::default()
        };
        let tiny_file = PdfFile::parse_with_limits(pdf, tiny_limits).expect("parse tiny");
        let tiny_font = load_single_font(&tiny_file, ObjectId(2, 0)).expect("load tiny");
        let tiny_has_fade = tiny_font
            .type3_resources()
            .and_then(|r| r.ext_g_state_smasks.get("GSFade"))
            .is_some_and(|sm| match sm {
                zpdf_font::Type3SoftMask::Dict { group, .. } => group
                    .resources
                    .as_ref()
                    .is_some_and(|r| r.shadings.contains_key("Fade")),
                zpdf_font::Type3SoftMask::None => false,
            });
        assert!(
            !tiny_has_fade,
            "oversized shading resource must be omitted under tight font budget"
        );
    }

    #[test]
    fn type3_loader_reuses_indirect_stitching_subfunction_within_tight_budget() {
        // Sampled Type 0 stream function with a 4096-byte payload (monotonic
        // ramp 0..=255) referenced TWICE by a Type 3 stitching function under
        // `/Functions [6 0 R 6 0 R]`. A 6500-byte font budget fits one payload
        // plus two interval entries, but cannot fit two decoded copies (>8 KB).
        let samples: Vec<u8> = (0..4096).map(|i| ((i * 255) / 4095) as u8).collect();
        let mut obj6 = format!(
            "<< /FunctionType 0 /Domain [0 1] /Range [0 1] /Size [4096] \
             /BitsPerSample 8 /Length {} >>\nstream\n",
            samples.len()
        )
        .into_bytes();
        obj6.extend_from_slice(&samples);
        obj6.extend_from_slice(b"\nendstream");

        let build_raw_pdf = |objs: &[Vec<u8>]| -> Vec<u8> {
            let mut out = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objs.len());
            for (i, body) in objs.iter().enumerate() {
                offsets.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                out.extend_from_slice(body);
                out.extend_from_slice(b"\nendobj\n");
            }
            let xref = out.len();
            let n = objs.len() + 1;
            out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(
                format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            out
        };

        let pdf = build_raw_pdf(&[
            b"<< /Type /Catalog >>".to_vec(), // 1
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 80 20] \
               /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 3 0 R >> \
               /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [80] \
               /Resources << /Shading << /SharedStitch 4 0 R >> >> >>"
                .to_vec(), // 2
            b"<< /Length 36 >>\nstream\n80 0 d0 0 0 80 20 re W n /SharedStitch sh\nendstream"
                .to_vec(), // 3
            b"<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 80 0] \
               /Function 5 0 R /Extend [true true] >>"
                .to_vec(), // 4
            b"<< /FunctionType 3 /Domain [0 1] /Bounds [0.5] /Encode [0 1 1 0] \
               /Functions [6 0 R 6 0 R] >>"
                .to_vec(), // 5
            obj6,                             // 6 (4096-byte shared sampled child)
        ]);

        let budget = 6_500u64;
        let limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: budget,
            ..Default::default()
        };
        let file = PdfFile::parse_with_limits(pdf, limits).expect("parse");
        let font = load_single_font(&file, ObjectId(2, 0)).expect("load type3");
        let res = font
            .type3_resources()
            .expect("Type 3 font must retain /Resources within 6500-byte budget");
        let shading = res
            .shadings
            .get("SharedStitch")
            .expect("stitching shading referencing one 4 KB sampled child twice must fit in 6500-byte budget");
        assert_eq!(shading.functions.len(), 1);
        let zpdf_font::Type3Function::Dict { children, .. } = shading.functions[0].as_ref() else {
            panic!("expected Type 3 stitching Dict function");
        };
        assert_eq!(children.len(), 2);
        assert!(
            std::sync::Arc::ptr_eq(&children[0], &children[1]),
            "both stitching intervals must share the same Arc<Type3Function>"
        );
        assert!(
            res.estimated_cache_bytes() <= budget,
            "estimated cache bytes {} must stay within budget {budget}",
            res.estimated_cache_bytes()
        );
    }

    #[test]
    fn type3_loader_rolls_back_cached_transfer_function_when_mask_group_rejected() {
        // `/XObject` runs before `/ExtGState`, which runs before `/Shading`.
        // 1. `/XObject /Keep` consumes ~2500 bytes (fits in 5000-byte budget).
        // 2. `/ExtGState /GSReject` has `/TR 6 0 R` (2000-byte sampled stream —
        //    fits in remaining ~2500!) followed by `/G 5 0 R` (10000-byte Form —
        //    exceeds remaining budget, rejecting the mask!).
        // 3. `/Shading /Later` references the SAME `6 0 R` (2000 bytes) AND
        //    `7 0 R` (1500 bytes). If `6 0 R` were NOT rolled back from
        //    `form_cache.functions` when `/G` failed, `/Later` would reuse
        //    `6 0 R` for free, admit both streams, and exceed the 5000-byte
        //    budget (`2500 + 2000 + 1500 = 6000 > 5000`).
        let keep_data = vec![b' '; 2_500];
        let big_group = vec![b' '; 10_000];
        let tr_samples = vec![128u8; 2_000];
        let extra_samples = vec![64u8; 1_500];

        let stream_obj = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut v = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            v.extend_from_slice(data);
            v.extend_from_slice(b"\nendstream");
            v
        };
        let build_raw_pdf = |objs: &[Vec<u8>]| -> Vec<u8> {
            let mut out = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::with_capacity(objs.len());
            for (i, body) in objs.iter().enumerate() {
                offsets.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
                out.extend_from_slice(body);
                out.extend_from_slice(b"\nendobj\n");
            }
            let xref = out.len();
            let n = objs.len() + 1;
            out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(
                format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            out
        };

        let pdf = build_raw_pdf(&[
            b"<< /Type /Catalog >>".to_vec(), // 1
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] \
               /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 3 0 R >> \
               /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [10] \
               /Resources << \
                 /XObject << /Keep 4 0 R >> \
                 /ExtGState << /GSReject << /SMask << /S /Luminosity /G 5 0 R /TR 6 0 R >> >> >> \
                 /Shading << /Later 8 0 R >> \
               >> >>"
                .to_vec(), // 2
            stream_obj("", b"10 0 d0 0 0 10 10 re f"), // 3
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 10 10]",
                &keep_data,
            ), // 4 (2500 B)
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 10 10]",
                &big_group,
            ), // 5 (10000 B)
            stream_obj(
                "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [2000] /BitsPerSample 8",
                &tr_samples,
            ), // 6 (2000 B)
            stream_obj(
                "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [1500] /BitsPerSample 8",
                &extra_samples,
            ), // 7 (1500 B)
            b"<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 10 0] \
               /Function << /FunctionType 3 /Domain [0 1] /Bounds [0.5] /Encode [0 1 0 1] \
                            /Functions [6 0 R 7 0 R] >> >>"
                .to_vec(), // 8
        ]);

        let budget = 5_000u64;
        let limits = zpdf_core::ParseLimits {
            max_font_cache_bytes: budget,
            ..Default::default()
        };
        let file = PdfFile::parse_with_limits(pdf, limits).expect("parse");
        let font = load_single_font(&file, ObjectId(2, 0)).expect("load type3");
        let res = font.type3_resources().expect("must retain /Keep");
        assert!(res.xobjects.contains_key("Keep"));
        assert!(
            !res.ext_g_state_smasks.contains_key("GSReject"),
            "oversized mask group must be rejected"
        );
        assert!(
            !res.shadings.contains_key("Later"),
            "rolled-back /TR function must not remain cached free for a later shading"
        );
        assert!(
            res.estimated_cache_bytes() <= budget,
            "retained Type 3 resource bytes {} exceeded budget {budget}",
            res.estimated_cache_bytes()
        );
    }

    #[test]
    fn type3_loader_enforces_function_depth_limit_equally_on_warm_and_cold_cache() {
        // Build a 6-level stitching chain `Deep` (height = 6):
        //   obj 10 (leaf, h=0) <- obj 11 (h=1) <- ... <- obj 16 (`Deep`, h=6)
        // Plus:
        //   - `MaxOk` (depth 0 -> 1 -> `Deep` at depth 2: 2 + 6 = 8 <= 8 -> ACCEPTED)
        //   - `TooDeep` (depth 0 -> 1 -> 2 -> `Deep` at depth 3: 3 + 6 = 9 > 8 -> REJECTED)
        // Compare cold cache (no `/ExtGState` warmup) vs warm cache (`/ExtGState
        // /GSWarm /SMask /TR 16 0 R` runs BEFORE `/Shading`, caching `Deep` at depth 0).
        let build_pdf = |warm_cache: bool| -> Vec<u8> {
            let ext_gs = if warm_cache {
                "/ExtGState << /GSWarm << /SMask << /S /Luminosity /G 4 0 R /TR 16 0 R >> >> >>"
            } else {
                ""
            };
            let mut objs: Vec<String> = vec![
                "<< /Type /Catalog >>".into(), // 1
                format!(
                    "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] \
                     /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 3 0 R >> \
                     /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [10] \
                     /Resources << {ext_gs} /Shading << /MaxOk 5 0 R /TooDeep 6 0 R >> >> >>"
                ), // 2
                "<< /Length 23 >>\nstream\n10 0 d0 0 0 10 10 re f\nendstream".into(), // 3
                "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] /Length 19 >>\n\
                 stream\n1 g 0 0 10 10 re f\nendstream"
                    .into(), // 4
                "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 10 0] \
                 /Function 18 0 R >>"
                    .into(), // 5 (`MaxOk`, root depth 0 -> obj 18 -> obj 17 -> obj 16)
                "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 10 0] \
                 /Function 19 0 R >>"
                    .into(), // 6 (`TooDeep`, root depth 0 -> obj 19 -> obj 18 -> obj 17 -> obj 16)
                "<< >>".into(),                // 7 (padding)
                "<< >>".into(),                // 8
                "<< >>".into(),                // 9
                // 10: leaf exponential function (height 0)
                "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>".into(),
            ];
            // 11..=16: 6 stitching wrappers (obj 16 has height 6).
            for prev in 10..=15 {
                objs.push(format!(
                    "<< /FunctionType 3 /Domain [0 1] /Bounds [] /Encode [0 1] \
                     /Functions [{prev} 0 R] >>"
                ));
            }
            // 17: height 7 (wraps 16)
            objs.push(
                "<< /FunctionType 3 /Domain [0 1] /Bounds [] /Encode [0 1] \
                 /Functions [16 0 R] >>"
                    .into(),
            );
            // 18: height 8 (wraps 17 -> `MaxOk` at depth 0 has total depth 8)
            objs.push(
                "<< /FunctionType 3 /Domain [0 1] /Bounds [] /Encode [0 1] \
                 /Functions [17 0 R] >>"
                    .into(),
            );
            // 19: height 9 (wraps 18 -> `TooDeep` at depth 0 has total depth 9)
            objs.push(
                "<< /FunctionType 3 /Domain [0 1] /Bounds [] /Encode [0 1] \
                 /Functions [18 0 R] >>"
                    .into(),
            );
            let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
            crate::test_util::build_pdf(&refs)
        };

        for (label, warm) in [("cold", false), ("warm", true)] {
            let file = PdfFile::parse(build_pdf(warm)).expect("parse");
            let font = load_single_font(&file, ObjectId(2, 0)).expect("load type3");
            let res = font.type3_resources().expect("resources");
            if warm {
                assert!(
                    res.ext_g_state_smasks.contains_key("GSWarm"),
                    "warm cache setup must retain /GSWarm"
                );
            }
            assert!(
                res.shadings.contains_key("MaxOk"),
                "{label} cache must accept total function nesting depth 8"
            );
            assert!(
                !res.shadings.contains_key("TooDeep"),
                "{label} cache must reject total function nesting depth 9"
            );
        }
    }
}
