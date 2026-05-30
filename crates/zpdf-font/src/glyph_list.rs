//! Adobe Glyph List (AGL) semantics: map a PostScript/PDF glyph name to its
//! Unicode value(s).
//!
//! This module implements the standard glyph-name-to-Unicode resolution used
//! during text extraction. It follows the algorithm described by Adobe's
//! "Unicode and Glyph Names" specification:
//!
//! 1. Strip any suffix after the first `.` (e.g. `"a.sc"` -> `"a"`).
//! 2. Look the (stripped) name up in a curated AGL table.
//! 3. Decode `uniXXXX[YYYY...]` names (each group is exactly 4 uppercase hex
//!    digits, a BMP code point; surrogate halves D800..DFFF are skipped).
//! 4. Decode `uXXXX`..`uXXXXXX` names (4 to 6 uppercase hex digits) into a
//!    single scalar value.
//! 5. Otherwise return `None`.
//!
//! The curated table covers every glyph name in the Latin text encodings
//! (StandardEncoding, WinAnsiEncoding, MacRomanEncoding, PDFDocEncoding), common
//! typographic punctuation and ligatures, the full Latin-1 letter set, and the
//! Greek alphabet plus a partial set of Symbol-font math glyphs. Less common
//! Symbol/ZapfDingbats names fall through to `None` (or the `uniXXXX` rules).

/// Glyph name -> Unicode string (usually 1 char; ligatures may be >1).
pub fn glyph_name_to_string(name: &str) -> Option<String> {
    // Step 1: strip suffix after the first '.'.
    // A leading '.' (e.g. ".notdef") has no base name; the part before the
    // first '.' is empty, which will fail every subsequent lookup -> None.
    let base = match name.find('.') {
        Some(idx) => &name[..idx],
        None => name,
    };

    if base.is_empty() {
        return None;
    }

    // Step 2: curated AGL table lookup.
    if let Some(s) = lookup_agl(base) {
        return Some(s.to_string());
    }

    // Step 3: "uniXXXX" (one or more 4-hex-digit groups).
    if let Some(rest) = base.strip_prefix("uni") {
        if !rest.is_empty() && rest.len() % 4 == 0 && is_all_upper_hex(rest) {
            let mut out = String::new();
            let mut ok = true;
            let mut any = false;
            let bytes = rest.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let group = &rest[i..i + 4];
                let cp = u32::from_str_radix(group, 16).expect("validated hex");
                // Skip surrogate halves per the AGL algorithm.
                if (0xD800..=0xDFFF).contains(&cp) {
                    i += 4;
                    continue;
                }
                match char::from_u32(cp) {
                    Some(c) => {
                        out.push(c);
                        any = true;
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
                i += 4;
            }
            if ok && any {
                return Some(out);
            }
            // If every group was a surrogate (nothing produced) or a group was
            // invalid, fall through to None for this branch. Since the prefix
            // was "uni" with valid hex grouping, no other rule applies.
            return None;
        }
    }

    // Step 4: "uXXXX".."uXXXXXX" (4 to 6 uppercase hex digits -> one scalar).
    if let Some(rest) = base.strip_prefix('u') {
        let len = rest.len();
        if (4..=6).contains(&len) && is_all_upper_hex(rest) {
            let cp = u32::from_str_radix(rest, 16).ok()?;
            if (0xD800..=0xDFFF).contains(&cp) {
                return None;
            }
            if let Some(c) = char::from_u32(cp) {
                return Some(c.to_string());
            }
            return None;
        }
    }

    None
}

/// Glyph name -> first Unicode scalar (for cmap lookup convenience).
pub fn glyph_name_to_char(name: &str) -> Option<char> {
    glyph_name_to_string(name).and_then(|s| s.chars().next())
}

/// Returns true iff every byte is a digit 0-9 or an uppercase hex letter A-F.
fn is_all_upper_hex(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

/// Curated Adobe Glyph List. Returns the Unicode string for a known glyph name.
///
/// The table is exhaustive enough to cover all of the standard PDF text
/// encodings plus the most common typographic glyph names.
fn lookup_agl(name: &str) -> Option<&'static str> {
    Some(match name {
        // ---- ASCII printable (U+0020..U+007E) ----
        "space" => " ",
        "exclam" => "!",
        "quotedbl" => "\"",
        "numbersign" => "#",
        "dollar" => "$",
        "percent" => "%",
        "ampersand" => "&",
        // quotesingle is the straight apostrophe U+0027; quoteright is the
        // typographic right single quote U+2019.
        "quotesingle" => "\u{0027}",
        "quoteright" => "\u{2019}",
        "quoteleft" => "\u{2018}",
        "parenleft" => "(",
        "parenright" => ")",
        "asterisk" => "*",
        "plus" => "+",
        "comma" => ",",
        "hyphen" => "-",
        "period" => ".",
        "slash" => "/",
        "zero" => "0",
        "one" => "1",
        "two" => "2",
        "three" => "3",
        "four" => "4",
        "five" => "5",
        "six" => "6",
        "seven" => "7",
        "eight" => "8",
        "nine" => "9",
        "colon" => ":",
        "semicolon" => ";",
        "less" => "<",
        "equal" => "=",
        "greater" => ">",
        "question" => "?",
        "at" => "@",
        "A" => "A",
        "B" => "B",
        "C" => "C",
        "D" => "D",
        "E" => "E",
        "F" => "F",
        "G" => "G",
        "H" => "H",
        "I" => "I",
        "J" => "J",
        "K" => "K",
        "L" => "L",
        "M" => "M",
        "N" => "N",
        "O" => "O",
        "P" => "P",
        "Q" => "Q",
        "R" => "R",
        "S" => "S",
        "T" => "T",
        "U" => "U",
        "V" => "V",
        "W" => "W",
        "X" => "X",
        "Y" => "Y",
        "Z" => "Z",
        "bracketleft" => "[",
        "backslash" => "\\",
        "bracketright" => "]",
        "asciicircum" => "^",
        "underscore" => "_",
        "grave" => "`", // spacing grave accent, U+0060
        "a" => "a",
        "b" => "b",
        "c" => "c",
        "d" => "d",
        "e" => "e",
        "f" => "f",
        "g" => "g",
        "h" => "h",
        "i" => "i",
        "j" => "j",
        "k" => "k",
        "l" => "l",
        "m" => "m",
        "n" => "n",
        "o" => "o",
        "p" => "p",
        "q" => "q",
        "r" => "r",
        "s" => "s",
        "t" => "t",
        "u" => "u",
        "v" => "v",
        "w" => "w",
        "x" => "x",
        "y" => "y",
        "z" => "z",
        "braceleft" => "{",
        "bar" => "|",
        "braceright" => "}",
        "asciitilde" => "~",

        // ---- Punctuation / symbols ----
        "quotedblleft" => "\u{201C}",
        "quotedblright" => "\u{201D}",
        "quotesinglbase" => "\u{201A}",
        "quotedblbase" => "\u{201E}",
        "bullet" => "\u{2022}",
        "endash" => "\u{2013}",
        "emdash" => "\u{2014}",
        "ellipsis" => "\u{2026}",
        "dagger" => "\u{2020}",
        "daggerdbl" => "\u{2021}",
        "perthousand" => "\u{2030}",
        "guilsinglleft" => "\u{2039}",
        "guilsinglright" => "\u{203A}",
        "guillemotleft" => "\u{00AB}",
        "guillemotright" => "\u{00BB}",
        "fraction" => "\u{2044}",
        "florin" => "\u{0192}",
        "section" => "\u{00A7}",
        "paragraph" => "\u{00B6}",
        "periodcentered" => "\u{00B7}",
        "dotlessi" => "\u{0131}",
        "dotlessj" => "\u{0237}",
        "trademark" => "\u{2122}",
        "copyright" => "\u{00A9}",
        "registered" => "\u{00AE}",
        "degree" => "\u{00B0}",
        "plusminus" => "\u{00B1}",
        "multiply" => "\u{00D7}",
        "divide" => "\u{00F7}",
        "logicalnot" => "\u{00AC}",
        "brokenbar" => "\u{00A6}",
        "currency" => "\u{00A4}",
        "cent" => "\u{00A2}",
        "sterling" => "\u{00A3}",
        "yen" => "\u{00A5}",
        "euro" => "\u{20AC}",
        "Euro" => "\u{20AC}",
        "exclamdown" => "\u{00A1}",
        "questiondown" => "\u{00BF}",
        "ordfeminine" => "\u{00AA}",
        "ordmasculine" => "\u{00BA}",
        "minus" => "\u{2212}",
        "onequarter" => "\u{00BC}",
        "onehalf" => "\u{00BD}",
        "threequarters" => "\u{00BE}",
        "onesuperior" => "\u{00B9}",
        "twosuperior" => "\u{00B2}",
        "threesuperior" => "\u{00B3}",
        "macron" => "\u{00AF}",
        "lozenge" => "\u{25CA}",
        "partialdiff" => "\u{2202}",
        "radical" => "\u{221A}",
        "infinity" => "\u{221E}",
        "integral" => "\u{222B}",
        "approxequal" => "\u{2248}",
        "notequal" => "\u{2260}",
        "lessequal" => "\u{2264}",
        "greaterequal" => "\u{2265}",
        "Delta" => "\u{0394}",
        "Omega" => "\u{03A9}",
        "summation" => "\u{2211}",
        "product" => "\u{220F}",
        "pi" => "\u{03C0}",

        // ---- Ligatures ----
        "fi" => "fi",
        "fl" => "fl",
        "ff" => "ff",
        "ffi" => "ffi",
        "ffl" => "ffl",

        // ---- Spacing diacritic / accent glyphs ----
        // (grave handled in ASCII block as U+0060)
        "acute" => "\u{00B4}",
        "circumflex" => "\u{02C6}",
        "tilde" => "\u{02DC}",
        "breve" => "\u{02D8}",
        "dotaccent" => "\u{02D9}",
        "dieresis" => "\u{00A8}",
        "ring" => "\u{02DA}",
        "cedilla" => "\u{00B8}",
        "hungarumlaut" => "\u{02DD}",
        "ogonek" => "\u{02DB}",
        "caron" => "\u{02C7}",

        // ---- Latin-1 / Latin Extended letters: uppercase ----
        "Agrave" => "\u{00C0}",
        "Aacute" => "\u{00C1}",
        "Acircumflex" => "\u{00C2}",
        "Atilde" => "\u{00C3}",
        "Adieresis" => "\u{00C4}",
        "Aring" => "\u{00C5}",
        "AE" => "\u{00C6}",
        "Ccedilla" => "\u{00C7}",
        "Egrave" => "\u{00C8}",
        "Eacute" => "\u{00C9}",
        "Ecircumflex" => "\u{00CA}",
        "Edieresis" => "\u{00CB}",
        "Igrave" => "\u{00CC}",
        "Iacute" => "\u{00CD}",
        "Icircumflex" => "\u{00CE}",
        "Idieresis" => "\u{00CF}",
        "Eth" => "\u{00D0}",
        "Ntilde" => "\u{00D1}",
        "Ograve" => "\u{00D2}",
        "Oacute" => "\u{00D3}",
        "Ocircumflex" => "\u{00D4}",
        "Otilde" => "\u{00D5}",
        "Odieresis" => "\u{00D6}",
        "Oslash" => "\u{00D8}",
        "Ugrave" => "\u{00D9}",
        "Uacute" => "\u{00DA}",
        "Ucircumflex" => "\u{00DB}",
        "Udieresis" => "\u{00DC}",
        "Yacute" => "\u{00DD}",
        "Thorn" => "\u{00DE}",
        "Scaron" => "\u{0160}",
        "Zcaron" => "\u{017D}",
        "Ydieresis" => "\u{0178}",
        "OE" => "\u{0152}",
        "Lslash" => "\u{0141}",

        // ---- Latin-1 / Latin Extended letters: lowercase ----
        "germandbls" => "\u{00DF}",
        "agrave" => "\u{00E0}",
        "aacute" => "\u{00E1}",
        "acircumflex" => "\u{00E2}",
        "atilde" => "\u{00E3}",
        "adieresis" => "\u{00E4}",
        "aring" => "\u{00E5}",
        "ae" => "\u{00E6}",
        "ccedilla" => "\u{00E7}",
        "egrave" => "\u{00E8}",
        "eacute" => "\u{00E9}",
        "ecircumflex" => "\u{00EA}",
        "edieresis" => "\u{00EB}",
        "igrave" => "\u{00EC}",
        "iacute" => "\u{00ED}",
        "icircumflex" => "\u{00EE}",
        "idieresis" => "\u{00EF}",
        "eth" => "\u{00F0}",
        "ntilde" => "\u{00F1}",
        "ograve" => "\u{00F2}",
        "oacute" => "\u{00F3}",
        "ocircumflex" => "\u{00F4}",
        "otilde" => "\u{00F5}",
        "odieresis" => "\u{00F6}",
        "oslash" => "\u{00F8}",
        "ugrave" => "\u{00F9}",
        "uacute" => "\u{00FA}",
        "ucircumflex" => "\u{00FB}",
        "udieresis" => "\u{00FC}",
        "yacute" => "\u{00FD}",
        "thorn" => "\u{00FE}",
        "ydieresis" => "\u{00FF}",
        "scaron" => "\u{0161}",
        "zcaron" => "\u{017E}",
        "oe" => "\u{0153}",
        "lslash" => "\u{0142}",

        // ---- Greek lowercase (for the Symbol font) ----
        "alpha" => "\u{03B1}",
        "beta" => "\u{03B2}",
        "gamma" => "\u{03B3}",
        "delta" => "\u{03B4}",
        "epsilon" => "\u{03B5}",
        "zeta" => "\u{03B6}",
        "eta" => "\u{03B7}",
        "theta" => "\u{03B8}",
        "iota" => "\u{03B9}",
        "kappa" => "\u{03BA}",
        "lambda" => "\u{03BB}",
        "mu" => "\u{00B5}", // AGL maps "mu" to MICRO SIGN U+00B5
        "nu" => "\u{03BD}",
        "xi" => "\u{03BE}",
        "omicron" => "\u{03BF}",
        // "pi" is handled in the symbols block (U+03C0).
        "rho" => "\u{03C1}",
        "sigma" => "\u{03C3}",
        "sigma1" => "\u{03C2}", // final sigma
        "tau" => "\u{03C4}",
        "upsilon" => "\u{03C5}",
        "phi" => "\u{03C6}",
        "chi" => "\u{03C7}",
        "psi" => "\u{03C8}",
        "omega" => "\u{03C9}",

        // ---- Greek uppercase ----
        "Alpha" => "\u{0391}",
        "Beta" => "\u{0392}",
        "Gamma" => "\u{0393}",
        // "Delta" handled in symbols block (U+0394).
        "Epsilon" => "\u{0395}",
        "Zeta" => "\u{0396}",
        "Eta" => "\u{0397}",
        "Theta" => "\u{0398}",
        "Iota" => "\u{0399}",
        "Kappa" => "\u{039A}",
        "Lambda" => "\u{039B}",
        "Mu" => "\u{039C}",
        "Nu" => "\u{039D}",
        "Xi" => "\u{039E}",
        "Omicron" => "\u{039F}",
        "Pi" => "\u{03A0}",
        "Rho" => "\u{03A1}",
        "Sigma" => "\u{03A3}",
        "Tau" => "\u{03A4}",
        "Upsilon" => "\u{03A5}",
        "Phi" => "\u{03A6}",
        "Chi" => "\u{03A7}",
        "Psi" => "\u{03A8}",
        // "Omega" handled in symbols block (U+03A9).
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ascii() {
        assert_eq!(glyph_name_to_string("A"), Some("A".to_string()));
        assert_eq!(glyph_name_to_string("space"), Some(" ".to_string()));
        assert_eq!(glyph_name_to_string("zero"), Some("0".to_string()));
        assert_eq!(glyph_name_to_string("asciitilde"), Some("~".to_string()));
        assert_eq!(glyph_name_to_string("at"), Some("@".to_string()));
    }

    #[test]
    fn typographic_punctuation() {
        assert_eq!(
            glyph_name_to_string("quoteleft"),
            Some("\u{2018}".to_string())
        );
        assert_eq!(
            glyph_name_to_string("quoteright"),
            Some("\u{2019}".to_string())
        );
        assert_eq!(
            glyph_name_to_string("quotesingle"),
            Some("\u{0027}".to_string())
        );
        assert_eq!(glyph_name_to_string("bullet"), Some("\u{2022}".to_string()));
        assert_eq!(glyph_name_to_string("endash"), Some("\u{2013}".to_string()));
        assert_eq!(glyph_name_to_string("emdash"), Some("\u{2014}".to_string()));
        assert_eq!(
            glyph_name_to_string("ellipsis"),
            Some("\u{2026}".to_string())
        );
        assert_eq!(
            glyph_name_to_string("trademark"),
            Some("\u{2122}".to_string())
        );
    }

    #[test]
    fn euro_both_cases() {
        assert_eq!(glyph_name_to_string("euro"), Some("\u{20AC}".to_string()));
        assert_eq!(glyph_name_to_string("Euro"), Some("\u{20AC}".to_string()));
    }

    #[test]
    fn ligatures() {
        assert_eq!(glyph_name_to_string("fi"), Some("fi".to_string()));
        assert_eq!(glyph_name_to_string("fl"), Some("fl".to_string()));
        assert_eq!(glyph_name_to_string("ff"), Some("ff".to_string()));
        assert_eq!(glyph_name_to_string("ffi"), Some("ffi".to_string()));
        assert_eq!(glyph_name_to_string("ffl"), Some("ffl".to_string()));
    }

    #[test]
    fn latin1_letters() {
        assert_eq!(glyph_name_to_string("aacute"), Some("\u{00E1}".to_string()));
        assert_eq!(
            glyph_name_to_string("Adieresis"),
            Some("\u{00C4}".to_string())
        );
        assert_eq!(
            glyph_name_to_string("germandbls"),
            Some("\u{00DF}".to_string())
        );
        assert_eq!(
            glyph_name_to_string("ydieresis"),
            Some("\u{00FF}".to_string())
        );
        assert_eq!(
            glyph_name_to_string("Ydieresis"),
            Some("\u{0178}".to_string())
        );
        assert_eq!(glyph_name_to_string("OE"), Some("\u{0152}".to_string()));
        assert_eq!(glyph_name_to_string("oe"), Some("\u{0153}".to_string()));
        assert_eq!(glyph_name_to_string("Lslash"), Some("\u{0141}".to_string()));
        assert_eq!(glyph_name_to_string("scaron"), Some("\u{0161}".to_string()));
    }

    #[test]
    fn greek() {
        assert_eq!(glyph_name_to_string("alpha"), Some("\u{03B1}".to_string()));
        assert_eq!(glyph_name_to_string("Omega"), Some("\u{03A9}".to_string()));
        assert_eq!(glyph_name_to_string("pi"), Some("\u{03C0}".to_string()));
        assert_eq!(glyph_name_to_string("Pi"), Some("\u{03A0}".to_string()));
        assert_eq!(glyph_name_to_string("Delta"), Some("\u{0394}".to_string()));
        // AGL "mu" is the micro sign, not Greek small letter mu.
        assert_eq!(glyph_name_to_string("mu"), Some("\u{00B5}".to_string()));
    }

    #[test]
    fn period_suffix_stripping() {
        assert_eq!(glyph_name_to_string("a.sc"), Some("a".to_string()));
        assert_eq!(glyph_name_to_string("A.alt01"), Some("A".to_string()));
        assert_eq!(glyph_name_to_string("space.sc"), Some(" ".to_string()));
        // Leading dot -> empty base -> None.
        assert_eq!(glyph_name_to_string(".notdef"), None);
    }

    #[test]
    fn uni_decoding() {
        // U+4E2D = 中
        assert_eq!(
            glyph_name_to_string("uni4E2D"),
            Some("\u{4E2D}".to_string())
        );
        assert_eq!(glyph_name_to_string("uni0041"), Some("A".to_string()));
        // Multiple groups concatenated.
        assert_eq!(glyph_name_to_string("uni00410042"), Some("AB".to_string()));
        // Surrogate half is skipped; with only a surrogate group the result is None.
        assert_eq!(glyph_name_to_string("uniD800"), None);
        // Mixed: valid + surrogate -> just the valid char.
        assert_eq!(glyph_name_to_string("uni0041D800"), Some("A".to_string()));
        // Wrong length (not a multiple of 4) -> None.
        assert_eq!(glyph_name_to_string("uni4E2"), None);
        // Lowercase hex is not accepted in uniXXXX form.
        assert_eq!(glyph_name_to_string("uni4e2d"), None);
    }

    #[test]
    fn u_decoding() {
        // 4 to 6 uppercase hex digits.
        assert_eq!(glyph_name_to_string("u4E2D"), Some("\u{4E2D}".to_string()));
        assert_eq!(
            glyph_name_to_string("u1F600"),
            Some("\u{1F600}".to_string())
        );
        assert_eq!(
            glyph_name_to_string("u10FFFF"),
            Some("\u{10FFFF}".to_string())
        );
        // Too short (3) and too long (7) -> None.
        assert_eq!(glyph_name_to_string("u4E2"), None);
        assert_eq!(glyph_name_to_string("u10FFFFF"), None);
        // Out of Unicode range -> None.
        assert_eq!(glyph_name_to_string("u110000"), None);
    }

    #[test]
    fn unknown_names() {
        assert!(glyph_name_to_string("bogusname123").is_none());
        assert!(glyph_name_to_string("").is_none());
        assert!(glyph_name_to_string("unihello").is_none());
    }

    #[test]
    fn char_helper() {
        assert_eq!(glyph_name_to_char("A"), Some('A'));
        assert_eq!(glyph_name_to_char("fi"), Some('f')); // first scalar of ligature
        assert_eq!(glyph_name_to_char("uni4E2D"), Some('\u{4E2D}'));
        assert_eq!(glyph_name_to_char("bogus"), None);
    }
}
