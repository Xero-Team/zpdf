//! System-font discovery for non-embedded PDF fonts.
//!
//! Non-embedded fonts (`/FontFile*` absent) otherwise render no glyphs: the
//! PDF carries only metrics. This module finds an installed substitute by
//! scanning the platform font directories once, indexing every face by
//! PostScript name, full name, and family + style, and resolving `/BaseFont`
//! names against that index: exact PostScript match first ("ArialMT",
//! "Arial-BoldMT"), then suffix-stripped family + style
//! ("TimesNewRomanPS-BoldMT" → "timesnewroman" + bold), then aliases
//! (Helvetica → Arial), then CJK defaults by CID ordering, and finally a
//! generic serif/sans/mono face.
//!
//! Scanning reads only each file's sfnt directory and `name` table via
//! partial reads (TTC collections enumerate every face); full file bytes are
//! loaded — and cached per path — only when a face is actually substituted.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// A face matched in the system-font index, ready to hand to
/// [`LoadedFont::new_substitute`](crate::LoadedFont::new_substitute).
#[derive(Debug, Clone)]
pub struct SystemFontMatch {
    /// Whole font-file bytes (shared; TTC collections are loaded once).
    pub data: Arc<[u8]>,
    /// Face index inside the file (0 for plain TTF/OTF).
    pub face_index: u32,
}

/// FontDescriptor-derived hints that guide substitution when the name alone
/// doesn't resolve.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubstituteHints {
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub fixed_pitch: bool,
}

/// Find a system font for a PDF `/BaseFont` name (subset prefixes are
/// stripped). `ordering` is the CID ordering ("GB1", "CNS1", "Japan1",
/// "Korea1") of a composite font, used to pick a CJK default when the name
/// itself doesn't match anything installed.
pub fn find_system_font(
    base_font: &str,
    hints: SubstituteHints,
    ordering: Option<&str>,
) -> Option<SystemFontMatch> {
    let index = font_index();
    if index.faces.is_empty() {
        return None;
    }

    let name = strip_subset_prefix(base_font);
    let (family_part, style_part) = split_family_style(name);
    let lower = name.to_ascii_lowercase();
    let bold = hints.bold
        || style_part.to_ascii_lowercase().contains("bold")
        || lower.contains("bold");
    let italic = hints.italic
        || {
            let sp = style_part.to_ascii_lowercase();
            sp.contains("italic") || sp.contains("oblique")
        }
        || lower.contains("italic")
        || lower.contains("oblique");
    let style = style_suffix(bold, italic);

    let full_norm = normalize(name);
    let family_norm = strip_ps_suffixes(&normalize(family_part));

    // Candidate keys, most to least specific. Each styled key also retries
    // without the style suffix — a regular face beats no face.
    let mut candidates: Vec<String> = Vec::new();
    let push = |candidates: &mut Vec<String>, base: &str| {
        if base.is_empty() {
            return;
        }
        if !style.is_empty() {
            candidates.push(format!("{base}{style}"));
        }
        candidates.push(base.to_string());
    };

    // The PS name itself (no style suffix games — it already encodes style).
    candidates.push(full_norm.clone());
    push(&mut candidates, &strip_ps_suffixes(&full_norm));
    push(&mut candidates, &family_norm);
    if let Some(alias) = alias_for(&family_norm) {
        push(&mut candidates, alias);
    }
    for fam in ordering_defaults(ordering) {
        push(&mut candidates, fam);
    }
    for fam in generic_defaults(hints) {
        push(&mut candidates, fam);
    }

    for key in &candidates {
        if let Some((path, face_index)) = index.faces.get(key.as_str()) {
            if let Some(data) = load_font_bytes(path) {
                tracing::debug!(
                    "substituting system font {} (face {face_index}) for {base_font}",
                    path.display()
                );
                return Some(SystemFontMatch {
                    data,
                    face_index: *face_index,
                });
            }
        }
    }
    tracing::debug!("no system font found for {base_font}");
    None
}

/// Strip a `ABCDEF+` subset prefix.
fn strip_subset_prefix(name: &str) -> &str {
    match name.split_once('+') {
        Some((prefix, rest))
            if prefix.len() == 6 && prefix.bytes().all(|b| b.is_ascii_uppercase()) =>
        {
            rest
        }
        _ => name,
    }
}

/// Split "Family-StyleTokens" / "Family,StyleTokens" at the first separator.
fn split_family_style(name: &str) -> (&str, &str) {
    match name.split_once(['-', ',']) {
        Some((fam, style)) => (fam, style),
        None => (name, ""),
    }
}

/// Lowercase alphanumerics only: "Times New Roman PS-MT" → "timesnewromanpsmt".
fn normalize(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Strip the PostScript-name suffixes Adobe/Monotype tack onto Windows core
/// fonts: "timesnewromanpsmt" → "timesnewroman", "arialmt" → "arial".
fn strip_ps_suffixes(norm: &str) -> String {
    let mut s = norm;
    loop {
        let before = s;
        for suffix in ["psmt", "ps", "mt"] {
            if let Some(stripped) = s.strip_suffix(suffix) {
                if !stripped.is_empty() {
                    s = stripped;
                }
            }
        }
        if s == before {
            return s.to_string();
        }
    }
}

fn style_suffix(bold: bool, italic: bool) -> &'static str {
    match (bold, italic) {
        (true, true) => "#bi",
        (true, false) => "#b",
        (false, true) => "#i",
        (false, false) => "",
    }
}

/// Well-known PDF base fonts → installed family (normalized).
fn alias_for(family_norm: &str) -> Option<&'static str> {
    Some(match family_norm {
        "helvetica" | "helveticaneue" | "arialmt" => "arial",
        "times" | "timesroman" | "timesnewromanps" => "timesnewroman",
        "courier" | "couriernewps" => "couriernew",
        // Common CJK PostScript families → Windows faces.
        "stsong" | "stsongstd" | "songti" | "songtisc" => "simsun",
        "stheiti" | "heiti" | "heitisc" => "simhei",
        "stkaiti" | "kaiti" | "kaitisc" => "kaiti",
        "stfangsong" | "fangsong" => "fangsong",
        "msmincho" | "hiraminprow3" | "hiraginominchopro" => "msmincho",
        "msgothic" | "hirakakuprow3" | "hiraginokakugothicpro" => "msgothic",
        "mingti" | "mingliu" => "mingliu",
        "batangche" => "batang",
        _ => return None,
    })
}

/// Default CJK families for a composite font's CID ordering.
fn ordering_defaults(ordering: Option<&str>) -> &'static [&'static str] {
    match ordering {
        Some("GB1") => &["microsoftyahei", "simsun", "simhei", "notosanscjksc"],
        Some("CNS1") => &["microsoftjhenghei", "mingliu", "pmingliu", "notosanscjktc"],
        Some("Japan1") => &["yugothic", "msgothic", "meiryo", "msmincho", "notosanscjkjp"],
        Some("Korea1" | "KR") => &["malgungothic", "batang", "gulim", "notosanscjkkr"],
        _ => &[],
    }
}

/// Last-resort families by descriptor hints.
fn generic_defaults(hints: SubstituteHints) -> &'static [&'static str] {
    if hints.fixed_pitch {
        &["couriernew", "consolas", "dejavusansmono", "liberationmono"]
    } else if hints.serif {
        &["timesnewroman", "georgia", "dejavuserif", "liberationserif", "notoserif"]
    } else {
        &["arial", "segoeui", "helvetica", "dejavusans", "liberationsans", "notosans"]
    }
}

// ---------------------------------------------------------------------------
// Index construction
// ---------------------------------------------------------------------------

struct FontIndex {
    /// normalized key → (file, face index). Keys: PS name, full name,
    /// family(+`#b`/`#i`/`#bi` style suffix), and the file stem.
    faces: HashMap<String, (PathBuf, u32)>,
}

fn font_index() -> &'static FontIndex {
    static INDEX: OnceLock<FontIndex> = OnceLock::new();
    INDEX.get_or_init(build_font_index)
}

/// Per-path cache of loaded font files, so a TTC shared by several PDF fonts
/// is read once.
fn load_font_bytes(path: &Path) -> Option<Arc<[u8]>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<[u8]>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().ok()?;
    if let Some(data) = cache.get(path) {
        return Some(data.clone());
    }
    let data: Arc<[u8]> = Arc::from(fs::read(path).ok()?);
    cache.insert(path.to_path_buf(), data.clone());
    Some(data)
}

fn build_font_index() -> FontIndex {
    let mut faces = HashMap::new();
    for dir in font_dirs() {
        scan_dir(&dir, 0, &mut faces);
    }
    tracing::debug!("system font index: {} keys", faces.len());
    FontIndex { faces }
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // Explicit override/extension (useful for tests and headless systems).
    if let Some(extra) = std::env::var_os("ZPDF_FONT_DIRS") {
        dirs.extend(std::env::split_paths(&extra));
    }

    #[cfg(target_os = "windows")]
    {
        let windir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        dirs.push(windir.join("Fonts"));
        if let Some(lad) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(lad).join(r"Microsoft\Windows\Fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/System/Library/Fonts/Supplemental"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".fonts"));
            dirs.push(home.join(".local/share/fonts"));
        }
    }

    dirs
}

fn scan_dir(dir: &Path, depth: usize, faces: &mut HashMap<String, (PathBuf, u32)>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, depth + 1, faces);
            continue;
        }
        let is_font = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc" | "otc"));
        if is_font {
            scan_font_file(&path, faces);
        }
    }
}

fn scan_font_file(path: &Path, faces: &mut HashMap<String, (PathBuf, u32)>) {
    let Ok(mut file) = fs::File::open(path) else {
        return;
    };
    let mut tag = [0u8; 4];
    if file.read_exact(&mut tag).is_err() {
        return;
    }

    let face_offsets: Vec<(u32, u64)> = if &tag == b"ttcf" {
        // TTC header: tag, version, numFonts, then numFonts × u32 offsets.
        let mut header = [0u8; 8];
        if file.read_exact(&mut header).is_err() {
            return;
        }
        let num_fonts = u32::from_be_bytes([header[4], header[5], header[6], header[7]]).min(64);
        let mut offsets = Vec::with_capacity(num_fonts as usize);
        let mut buf = [0u8; 4];
        for i in 0..num_fonts {
            if file.read_exact(&mut buf).is_err() {
                return;
            }
            offsets.push((i, u32::from_be_bytes(buf) as u64));
        }
        offsets
    } else if matches!(&tag, b"\x00\x01\x00\x00" | b"OTTO" | b"true" | b"typ1") {
        vec![(0, 0)]
    } else {
        return;
    };

    for (face_index, sfnt_offset) in face_offsets {
        if let Some(names) = read_face_names(&mut file, sfnt_offset) {
            insert_face(faces, path, face_index, &names);
        }
    }

    // The file stem is a handy alias ("msyh" → Microsoft YaHei face 0).
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let key = normalize(stem);
        if !key.is_empty() {
            faces.entry(key).or_insert_with(|| (path.to_path_buf(), 0));
        }
    }
}

struct FaceNames {
    ps_name: Option<String>,
    full_name: Option<String>,
    family: Option<String>,
    subfamily: Option<String>,
    typo_family: Option<String>,
    typo_subfamily: Option<String>,
}

fn insert_face(
    faces: &mut HashMap<String, (PathBuf, u32)>,
    path: &Path,
    face_index: u32,
    names: &FaceNames,
) {
    let mut add = |key: String| {
        if !key.is_empty() {
            // First wins: earlier directories (and earlier faces) take priority.
            faces
                .entry(key)
                .or_insert_with(|| (path.to_path_buf(), face_index));
        }
    };

    if let Some(ps) = &names.ps_name {
        add(normalize(ps));
    }
    if let Some(full) = &names.full_name {
        add(normalize(full));
    }
    for (family, subfamily) in [
        (&names.typo_family, &names.typo_subfamily),
        (&names.family, &names.subfamily),
    ] {
        if let Some(fam) = family {
            let sub = subfamily.as_deref().unwrap_or("").to_ascii_lowercase();
            let bold = sub.contains("bold");
            let italic = sub.contains("italic") || sub.contains("oblique");
            add(format!("{}{}", normalize(fam), style_suffix(bold, italic)));
        }
    }
}

/// Read the `name` table of the face whose sfnt directory starts at
/// `sfnt_offset`, with bounded partial reads only.
fn read_face_names(file: &mut fs::File, sfnt_offset: u64) -> Option<FaceNames> {
    let mut header = [0u8; 12];
    file.seek(SeekFrom::Start(sfnt_offset)).ok()?;
    file.read_exact(&mut header).ok()?;
    let num_tables = u16::from_be_bytes([header[4], header[5]]).min(512) as usize;

    let mut directory = vec![0u8; num_tables * 16];
    file.read_exact(&mut directory).ok()?;

    let mut name_table: Option<(u64, usize)> = None;
    for rec in directory.chunks_exact(16) {
        if &rec[0..4] == b"name" {
            let offset = u32::from_be_bytes([rec[8], rec[9], rec[10], rec[11]]) as u64;
            let length = u32::from_be_bytes([rec[12], rec[13], rec[14], rec[15]]) as usize;
            // Table offsets are absolute file offsets, also for TTC faces.
            name_table = Some((offset, length.min(1 << 20)));
            break;
        }
    }
    let (offset, length) = name_table?;
    if length < 6 {
        return None;
    }
    let mut table = vec![0u8; length];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut table).ok()?;

    Some(parse_name_table(&table))
}

fn parse_name_table(table: &[u8]) -> FaceNames {
    let mut names = FaceNames {
        ps_name: None,
        full_name: None,
        family: None,
        subfamily: None,
        typo_family: None,
        typo_subfamily: None,
    };
    if table.len() < 6 {
        return names;
    }
    let count = u16::from_be_bytes([table[2], table[3]]) as usize;
    let string_offset = u16::from_be_bytes([table[4], table[5]]) as usize;

    // (slot, current priority); higher priority wins.
    let mut best: HashMap<u16, u8> = HashMap::new();

    for i in 0..count {
        let rec = 6 + i * 12;
        if rec + 12 > table.len() {
            break;
        }
        let platform = u16::from_be_bytes([table[rec], table[rec + 1]]);
        let encoding = u16::from_be_bytes([table[rec + 2], table[rec + 3]]);
        let language = u16::from_be_bytes([table[rec + 4], table[rec + 5]]);
        let name_id = u16::from_be_bytes([table[rec + 6], table[rec + 7]]);
        let len = u16::from_be_bytes([table[rec + 8], table[rec + 9]]) as usize;
        let off = u16::from_be_bytes([table[rec + 10], table[rec + 11]]) as usize;

        if !matches!(name_id, 1 | 2 | 4 | 6 | 16 | 17) {
            continue;
        }

        // Prefer Windows en-US Unicode, then any Windows Unicode, then
        // Unicode-platform, then Mac Roman.
        let priority = match (platform, encoding) {
            (3, 1) | (3, 10) if language == 0x409 => 4,
            (3, 1) | (3, 10) => 3,
            (0, _) => 2,
            (1, 0) => 1,
            _ => continue,
        };
        if best.get(&name_id).copied().unwrap_or(0) >= priority {
            continue;
        }

        let start = string_offset + off;
        let Some(bytes) = table.get(start..start + len) else {
            continue;
        };
        let value = if platform == 1 {
            // Mac Roman ≈ ASCII for the names we care about.
            bytes.iter().map(|&b| b as char).collect::<String>()
        } else {
            // UTF-16BE.
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        };
        let value = value.trim().to_string();
        if value.is_empty() {
            continue;
        }
        best.insert(name_id, priority);
        match name_id {
            1 => names.family = Some(value),
            2 => names.subfamily = Some(value),
            4 => names.full_name = Some(value),
            6 => names.ps_name = Some(value),
            16 => names.typo_family = Some(value),
            17 => names.typo_subfamily = Some(value),
            _ => {}
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_prefix_stripping() {
        assert_eq!(strip_subset_prefix("ABCDEF+ArialMT"), "ArialMT");
        assert_eq!(strip_subset_prefix("ArialMT"), "ArialMT");
        // Lowercase / short prefixes are not subset tags.
        assert_eq!(strip_subset_prefix("Abc+Foo"), "Abc+Foo");
        assert_eq!(strip_subset_prefix("AB+Foo"), "AB+Foo");
    }

    #[test]
    fn name_normalization() {
        assert_eq!(normalize("Times New Roman PS-MT"), "timesnewromanpsmt");
        assert_eq!(strip_ps_suffixes("timesnewromanpsmt"), "timesnewroman");
        assert_eq!(strip_ps_suffixes("arialmt"), "arial");
        assert_eq!(strip_ps_suffixes("courierneWps".to_ascii_lowercase().as_str()), "couriernew");
    }

    #[test]
    fn family_style_splitting() {
        assert_eq!(split_family_style("Arial-BoldMT"), ("Arial", "BoldMT"));
        assert_eq!(split_family_style("Frutiger,Italic"), ("Frutiger", "Italic"));
        assert_eq!(split_family_style("Verdana"), ("Verdana", ""));
    }

    #[test]
    fn alias_resolution() {
        assert_eq!(alias_for("helvetica"), Some("arial"));
        assert_eq!(alias_for("times"), Some("timesnewroman"));
        assert_eq!(alias_for("courier"), Some("couriernew"));
        assert_eq!(alias_for("verdana"), None);
    }

    #[test]
    fn name_table_parsing_prefers_windows_unicode() {
        // Two records for nameID 6: Mac Roman "MacName", Windows en-US "WinName".
        let mac = b"MacName";
        let win: Vec<u8> = "WinName".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        let mut table = Vec::new();
        table.extend_from_slice(&0u16.to_be_bytes()); // format
        table.extend_from_slice(&2u16.to_be_bytes()); // count
        let string_offset = 6 + 2 * 12;
        table.extend_from_slice(&(string_offset as u16).to_be_bytes());
        // record 1: Mac Roman (1,0), lang 0, nameID 6
        for v in [1u16, 0, 0, 6, mac.len() as u16, 0] {
            table.extend_from_slice(&v.to_be_bytes());
        }
        // record 2: Windows (3,1), lang 0x409, nameID 6
        for v in [3u16, 1, 0x409, 6, win.len() as u16, mac.len() as u16] {
            table.extend_from_slice(&v.to_be_bytes());
        }
        table.extend_from_slice(mac);
        table.extend_from_slice(&win);

        let names = parse_name_table(&table);
        assert_eq!(names.ps_name.as_deref(), Some("WinName"));
    }

    #[test]
    #[cfg(windows)]
    fn windows_index_finds_arial() {
        // Arial ships with every Windows since 3.1; if this fails the scanner
        // is broken, not the machine.
        let m = find_system_font("ArialMT", SubstituteHints::default(), None);
        assert!(m.is_some(), "ArialMT should resolve on Windows");
        let m = find_system_font("Arial-BoldMT", SubstituteHints::default(), None);
        assert!(m.is_some(), "Arial-BoldMT should resolve on Windows");
        let m = find_system_font("Helvetica", SubstituteHints::default(), None);
        assert!(m.is_some(), "Helvetica should alias to Arial on Windows");
    }
}
