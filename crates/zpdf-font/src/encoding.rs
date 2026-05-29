//! Predefined PDF text-string encodings.
//!
//! Each encoding maps a single-byte character code (`0..=255`) to a PostScript
//! glyph name, per PDF 32000-1:2008 Annex D (StandardEncoding, WinAnsiEncoding,
//! MacRomanEncoding, PDFDocEncoding) and Annex D.4/D.5 (Symbol, ZapfDingbats).
//!
//! A per-font [`Encoding`] is a base table plus optional `/Differences`.

use std::borrow::Cow;

/// Maps a character code (`0..=255`) to a glyph name (PostScript name).
/// `None` means the code is undefined in this encoding.
pub type EncodingTable = [Option<&'static str>; 256];

// ---------------------------------------------------------------------------
// Helper builders
// ---------------------------------------------------------------------------

/// The printable ASCII region (codes 32..=126) that is *shared* by
/// StandardEncoding, WinAnsiEncoding, MacRomanEncoding and PDFDocEncoding.
///
/// Two codes differ between Standard and the others:
/// * code 39 — "quotesingle" (Win/Mac/PDFDoc) vs "quoteright" (Standard)
/// * code 96 — "grave"       (Win/Mac/PDFDoc) vs "quoteleft"  (Standard)
///
/// This table holds the WinAnsi/Mac/PDFDoc spelling (quotesingle / grave).
const ASCII_COMMON: [(u8, &str); 95] = [
    (32, "space"),
    (33, "exclam"),
    (34, "quotedbl"),
    (35, "numbersign"),
    (36, "dollar"),
    (37, "percent"),
    (38, "ampersand"),
    (39, "quotesingle"),
    (40, "parenleft"),
    (41, "parenright"),
    (42, "asterisk"),
    (43, "plus"),
    (44, "comma"),
    (45, "hyphen"),
    (46, "period"),
    (47, "slash"),
    (48, "zero"),
    (49, "one"),
    (50, "two"),
    (51, "three"),
    (52, "four"),
    (53, "five"),
    (54, "six"),
    (55, "seven"),
    (56, "eight"),
    (57, "nine"),
    (58, "colon"),
    (59, "semicolon"),
    (60, "less"),
    (61, "equal"),
    (62, "greater"),
    (63, "question"),
    (64, "at"),
    (65, "A"),
    (66, "B"),
    (67, "C"),
    (68, "D"),
    (69, "E"),
    (70, "F"),
    (71, "G"),
    (72, "H"),
    (73, "I"),
    (74, "J"),
    (75, "K"),
    (76, "L"),
    (77, "M"),
    (78, "N"),
    (79, "O"),
    (80, "P"),
    (81, "Q"),
    (82, "R"),
    (83, "S"),
    (84, "T"),
    (85, "U"),
    (86, "V"),
    (87, "W"),
    (88, "X"),
    (89, "Y"),
    (90, "Z"),
    (91, "bracketleft"),
    (92, "backslash"),
    (93, "bracketright"),
    (94, "asciicircum"),
    (95, "underscore"),
    (96, "grave"),
    (97, "a"),
    (98, "b"),
    (99, "c"),
    (100, "d"),
    (101, "e"),
    (102, "f"),
    (103, "g"),
    (104, "h"),
    (105, "i"),
    (106, "j"),
    (107, "k"),
    (108, "l"),
    (109, "m"),
    (110, "n"),
    (111, "o"),
    (112, "p"),
    (113, "q"),
    (114, "r"),
    (115, "s"),
    (116, "t"),
    (117, "u"),
    (118, "v"),
    (119, "w"),
    (120, "x"),
    (121, "y"),
    (122, "z"),
    (123, "braceleft"),
    (124, "bar"),
    (125, "braceright"),
    (126, "asciitilde"),
];

/// Build an all-`None` table and apply the supplied `(code, name)` pairs.
const fn build_from(pairs: &[(u8, &'static str)]) -> EncodingTable {
    let mut table: EncodingTable = [None; 256];
    let mut i = 0;
    while i < pairs.len() {
        let (code, name) = pairs[i];
        table[code as usize] = Some(name);
        i += 1;
    }
    table
}

/// Seed a table with the shared printable-ASCII region, then apply extras.
/// `standard_quotes` chooses the Standard spelling of codes 39 and 96.
const fn build_ascii_based(
    standard_quotes: bool,
    extras: &[(u8, &'static str)],
) -> EncodingTable {
    let mut table = build_from(&ASCII_COMMON);
    if standard_quotes {
        table[39] = Some("quoteright");
        table[96] = Some("quoteleft");
    }
    let mut i = 0;
    while i < extras.len() {
        let (code, name) = extras[i];
        table[code as usize] = Some(name);
        i += 1;
    }
    table
}

// ---------------------------------------------------------------------------
// StandardEncoding (PDF Annex D.2, "STD" column)
// ---------------------------------------------------------------------------

/// Adobe StandardEncoding high-range entries (codes 128..=255).
const STANDARD_HIGH: [(u8, &str); 54] = [
    (161, "exclamdown"),
    (162, "cent"),
    (163, "sterling"),
    (164, "fraction"),
    (165, "yen"),
    (166, "florin"),
    (167, "section"),
    (168, "currency"),
    (169, "quotesingle"),
    (170, "quotedblleft"),
    (171, "guillemotleft"),
    (172, "guilsinglleft"),
    (173, "guilsinglright"),
    (174, "fi"),
    (175, "fl"),
    (177, "endash"),
    (178, "dagger"),
    (179, "daggerdbl"),
    (180, "periodcentered"),
    (182, "paragraph"),
    (183, "bullet"),
    (184, "quotesinglbase"),
    (185, "quotedblbase"),
    (186, "quotedblright"),
    (187, "guillemotright"),
    (188, "ellipsis"),
    (189, "perthousand"),
    (191, "questiondown"),
    (193, "grave"),
    (194, "acute"),
    (195, "circumflex"),
    (196, "tilde"),
    (197, "macron"),
    (198, "breve"),
    (199, "dotaccent"),
    (200, "dieresis"),
    (202, "ring"),
    (203, "cedilla"),
    (205, "hungarumlaut"),
    (206, "ogonek"),
    (207, "caron"),
    (208, "emdash"),
    (225, "AE"),
    (227, "ordfeminine"),
    (232, "Lslash"),
    (233, "Oslash"),
    (234, "OE"),
    (235, "ordmasculine"),
    (241, "ae"),
    (245, "dotlessi"),
    (248, "lslash"),
    (249, "oslash"),
    (250, "oe"),
    (251, "germandbls"),
];

/// PDF StandardEncoding table (Annex D).
pub static STANDARD_ENCODING: EncodingTable = build_standard();

const fn build_standard() -> EncodingTable {
    // Start from ASCII with Standard quote spellings (39->quoteright, 96->quoteleft).
    let mut table = build_ascii_based(true, &[]);
    let mut i = 0;
    while i < STANDARD_HIGH.len() {
        let (code, name) = STANDARD_HIGH[i];
        table[code as usize] = Some(name);
        i += 1;
    }
    table
}

// ---------------------------------------------------------------------------
// WinAnsiEncoding (Windows-1252, PDF Annex D)
// ---------------------------------------------------------------------------

/// WinAnsiEncoding high-range entries (codes 128..=255).
const WIN_ANSI_HIGH: [(u8, &str); 123] = [
    (128, "Euro"),
    (130, "quotesinglbase"),
    (131, "florin"),
    (132, "quotedblbase"),
    (133, "ellipsis"),
    (134, "dagger"),
    (135, "daggerdbl"),
    (136, "circumflex"),
    (137, "perthousand"),
    (138, "Scaron"),
    (139, "guilsinglleft"),
    (140, "OE"),
    (142, "Zcaron"),
    (145, "quoteleft"),
    (146, "quoteright"),
    (147, "quotedblleft"),
    (148, "quotedblright"),
    (149, "bullet"),
    (150, "endash"),
    (151, "emdash"),
    (152, "tilde"),
    (153, "trademark"),
    (154, "scaron"),
    (155, "guilsinglright"),
    (156, "oe"),
    (158, "zcaron"),
    (159, "Ydieresis"),
    (160, "space"),
    (161, "exclamdown"),
    (162, "cent"),
    (163, "sterling"),
    (164, "currency"),
    (165, "yen"),
    (166, "brokenbar"),
    (167, "section"),
    (168, "dieresis"),
    (169, "copyright"),
    (170, "ordfeminine"),
    (171, "guillemotleft"),
    (172, "logicalnot"),
    (173, "hyphen"),
    (174, "registered"),
    (175, "macron"),
    (176, "degree"),
    (177, "plusminus"),
    (178, "twosuperior"),
    (179, "threesuperior"),
    (180, "acute"),
    (181, "mu"),
    (182, "paragraph"),
    (183, "periodcentered"),
    (184, "cedilla"),
    (185, "onesuperior"),
    (186, "ordmasculine"),
    (187, "guillemotright"),
    (188, "onequarter"),
    (189, "onehalf"),
    (190, "threequarters"),
    (191, "questiondown"),
    (192, "Agrave"),
    (193, "Aacute"),
    (194, "Acircumflex"),
    (195, "Atilde"),
    (196, "Adieresis"),
    (197, "Aring"),
    (198, "AE"),
    (199, "Ccedilla"),
    (200, "Egrave"),
    (201, "Eacute"),
    (202, "Ecircumflex"),
    (203, "Edieresis"),
    (204, "Igrave"),
    (205, "Iacute"),
    (206, "Icircumflex"),
    (207, "Idieresis"),
    (208, "Eth"),
    (209, "Ntilde"),
    (210, "Ograve"),
    (211, "Oacute"),
    (212, "Ocircumflex"),
    (213, "Otilde"),
    (214, "Odieresis"),
    (215, "multiply"),
    (216, "Oslash"),
    (217, "Ugrave"),
    (218, "Uacute"),
    (219, "Ucircumflex"),
    (220, "Udieresis"),
    (221, "Yacute"),
    (222, "Thorn"),
    (223, "germandbls"),
    (224, "agrave"),
    (225, "aacute"),
    (226, "acircumflex"),
    (227, "atilde"),
    (228, "adieresis"),
    (229, "aring"),
    (230, "ae"),
    (231, "ccedilla"),
    (232, "egrave"),
    (233, "eacute"),
    (234, "ecircumflex"),
    (235, "edieresis"),
    (236, "igrave"),
    (237, "iacute"),
    (238, "icircumflex"),
    (239, "idieresis"),
    (240, "eth"),
    (241, "ntilde"),
    (242, "ograve"),
    (243, "oacute"),
    (244, "ocircumflex"),
    (245, "otilde"),
    (246, "odieresis"),
    (247, "divide"),
    (248, "oslash"),
    (249, "ugrave"),
    (250, "uacute"),
    (251, "ucircumflex"),
    (252, "udieresis"),
    (253, "yacute"),
    (254, "thorn"),
    (255, "ydieresis"),
];

/// PDF WinAnsiEncoding table (Windows-1252).
pub static WIN_ANSI_ENCODING: EncodingTable = build_win_ansi();

const fn build_win_ansi() -> EncodingTable {
    // ASCII region with WinAnsi quote spellings (39->quotesingle, 96->grave).
    let mut table = build_ascii_based(false, &[]);
    let mut i = 0;
    while i < WIN_ANSI_HIGH.len() {
        let (code, name) = WIN_ANSI_HIGH[i];
        table[code as usize] = Some(name);
        i += 1;
    }
    table
}

// ---------------------------------------------------------------------------
// MacRomanEncoding (Mac OS Roman, PDF Annex D)
// ---------------------------------------------------------------------------

/// MacRomanEncoding high-range entries (codes 128..=255), per PDF Annex D.
const MAC_ROMAN_HIGH: [(u8, &str); 128] = [
    (128, "Adieresis"),
    (129, "Aring"),
    (130, "Ccedilla"),
    (131, "Eacute"),
    (132, "Ntilde"),
    (133, "Odieresis"),
    (134, "Udieresis"),
    (135, "aacute"),
    (136, "agrave"),
    (137, "acircumflex"),
    (138, "adieresis"),
    (139, "atilde"),
    (140, "aring"),
    (141, "ccedilla"),
    (142, "eacute"),
    (143, "egrave"),
    (144, "ecircumflex"),
    (145, "edieresis"),
    (146, "iacute"),
    (147, "igrave"),
    (148, "icircumflex"),
    (149, "idieresis"),
    (150, "ntilde"),
    (151, "oacute"),
    (152, "ograve"),
    (153, "ocircumflex"),
    (154, "odieresis"),
    (155, "otilde"),
    (156, "uacute"),
    (157, "ugrave"),
    (158, "ucircumflex"),
    (159, "udieresis"),
    (160, "dagger"),
    (161, "degree"),
    (162, "cent"),
    (163, "sterling"),
    (164, "section"),
    (165, "bullet"),
    (166, "paragraph"),
    (167, "germandbls"),
    (168, "registered"),
    (169, "copyright"),
    (170, "trademark"),
    (171, "acute"),
    (172, "dieresis"),
    (173, "notequal"),
    (174, "AE"),
    (175, "Oslash"),
    (176, "infinity"),
    (177, "plusminus"),
    (178, "lessequal"),
    (179, "greaterequal"),
    (180, "yen"),
    (181, "mu"),
    (182, "partialdiff"),
    (183, "summation"),
    (184, "product"),
    (185, "pi"),
    (186, "integral"),
    (187, "ordfeminine"),
    (188, "ordmasculine"),
    (189, "Omega"),
    (190, "ae"),
    (191, "oslash"),
    (192, "questiondown"),
    (193, "exclamdown"),
    (194, "logicalnot"),
    (195, "radical"),
    (196, "florin"),
    (197, "approxequal"),
    (198, "Delta"),
    (199, "guillemotleft"),
    (200, "guillemotright"),
    (201, "ellipsis"),
    (202, "space"),
    (203, "Agrave"),
    (204, "Atilde"),
    (205, "Otilde"),
    (206, "OE"),
    (207, "oe"),
    (208, "endash"),
    (209, "emdash"),
    (210, "quotedblleft"),
    (211, "quotedblright"),
    (212, "quoteleft"),
    (213, "quoteright"),
    (214, "divide"),
    (215, "lozenge"),
    (216, "ydieresis"),
    (217, "Ydieresis"),
    (218, "fraction"),
    (219, "currency"),
    (220, "guilsinglleft"),
    (221, "guilsinglright"),
    (222, "fi"),
    (223, "fl"),
    (224, "daggerdbl"),
    (225, "periodcentered"),
    (226, "quotesinglbase"),
    (227, "quotedblbase"),
    (228, "perthousand"),
    (229, "Acircumflex"),
    (230, "Ecircumflex"),
    (231, "Aacute"),
    (232, "Edieresis"),
    (233, "Egrave"),
    (234, "Iacute"),
    (235, "Icircumflex"),
    (236, "Idieresis"),
    (237, "Igrave"),
    (238, "Oacute"),
    (239, "Ocircumflex"),
    (240, "apple"),
    (241, "Ograve"),
    (242, "Uacute"),
    (243, "Ucircumflex"),
    (244, "Ugrave"),
    (245, "dotlessi"),
    (246, "circumflex"),
    (247, "tilde"),
    (248, "macron"),
    (249, "breve"),
    (250, "dotaccent"),
    (251, "ring"),
    (252, "cedilla"),
    (253, "hungarumlaut"),
    (254, "ogonek"),
    (255, "caron"),
];

/// PDF MacRomanEncoding table (Mac OS Roman).
pub static MAC_ROMAN_ENCODING: EncodingTable = build_mac_roman();

const fn build_mac_roman() -> EncodingTable {
    let mut table = build_ascii_based(false, &[]);
    let mut i = 0;
    while i < MAC_ROMAN_HIGH.len() {
        let (code, name) = MAC_ROMAN_HIGH[i];
        table[code as usize] = Some(name);
        i += 1;
    }
    table
}

// ---------------------------------------------------------------------------
// PDFDocEncoding (PDF Annex D.2, "PDFDoc" column)
// ---------------------------------------------------------------------------

/// PDFDocEncoding high-range entries. PDFDoc shares WinAnsi's Latin-1 upper
/// half (codes 160..=255) but defines a distinct set in 128..=159 and a few
/// control-range glyphs (0x18..0x1F) used for typographic marks.
const PDF_DOC_HIGH: [(u8, &str); 134] = [
    // Control-range typographic glyphs defined by PDFDocEncoding (D.2).
    (0x18, "breve"),
    (0x19, "caron"),
    (0x1A, "circumflex"),
    (0x1B, "dotaccent"),
    (0x1C, "hungarumlaut"),
    (0x1D, "ogonek"),
    (0x1E, "ring"),
    (0x1F, "tilde"),
    // 0x80..0x9F region.
    (128, "bullet"),
    (129, "dagger"),
    (130, "daggerdbl"),
    (131, "ellipsis"),
    (132, "emdash"),
    (133, "endash"),
    (134, "florin"),
    (135, "fraction"),
    (136, "guilsinglleft"),
    (137, "guilsinglright"),
    (138, "minus"),
    (139, "perthousand"),
    (140, "quotedblbase"),
    (141, "quotedblleft"),
    (142, "quotedblright"),
    (143, "quoteleft"),
    (144, "quoteright"),
    (145, "quotesinglbase"),
    (146, "trademark"),
    (147, "fi"),
    (148, "fl"),
    (149, "Lslash"),
    (150, "OE"),
    (151, "Scaron"),
    (152, "Ydieresis"),
    (153, "Zcaron"),
    (154, "dotlessi"),
    (155, "lslash"),
    (156, "oe"),
    (157, "scaron"),
    (158, "zcaron"),
    // 159 (0x9F) is undefined in PDFDocEncoding.
    (160, "Euro"),
    // 160 is "Euro" in PDFDoc (per ISO 32000), unlike WinAnsi where 160=space.
    (161, "exclamdown"),
    (162, "cent"),
    (163, "sterling"),
    (164, "currency"),
    (165, "yen"),
    (166, "brokenbar"),
    (167, "section"),
    (168, "dieresis"),
    (169, "copyright"),
    (170, "ordfeminine"),
    (171, "guillemotleft"),
    (172, "logicalnot"),
    // 173 (0xAD, soft hyphen) is undefined in PDFDocEncoding (unlike WinAnsi).
    (174, "registered"),
    (175, "macron"),
    (176, "degree"),
    (177, "plusminus"),
    (178, "twosuperior"),
    (179, "threesuperior"),
    (180, "acute"),
    (181, "mu"),
    (182, "paragraph"),
    (183, "periodcentered"),
    (184, "cedilla"),
    (185, "onesuperior"),
    (186, "ordmasculine"),
    (187, "guillemotright"),
    (188, "onequarter"),
    (189, "onehalf"),
    (190, "threequarters"),
    (191, "questiondown"),
    (192, "Agrave"),
    (193, "Aacute"),
    (194, "Acircumflex"),
    (195, "Atilde"),
    (196, "Adieresis"),
    (197, "Aring"),
    (198, "AE"),
    (199, "Ccedilla"),
    (200, "Egrave"),
    (201, "Eacute"),
    (202, "Ecircumflex"),
    (203, "Edieresis"),
    (204, "Igrave"),
    (205, "Iacute"),
    (206, "Icircumflex"),
    (207, "Idieresis"),
    (208, "Eth"),
    (209, "Ntilde"),
    (210, "Ograve"),
    (211, "Oacute"),
    (212, "Ocircumflex"),
    (213, "Otilde"),
    (214, "Odieresis"),
    (215, "multiply"),
    (216, "Oslash"),
    (217, "Ugrave"),
    (218, "Uacute"),
    (219, "Ucircumflex"),
    (220, "Udieresis"),
    (221, "Yacute"),
    (222, "Thorn"),
    (223, "germandbls"),
    (224, "agrave"),
    (225, "aacute"),
    (226, "acircumflex"),
    (227, "atilde"),
    (228, "adieresis"),
    (229, "aring"),
    (230, "ae"),
    (231, "ccedilla"),
    (232, "egrave"),
    (233, "eacute"),
    (234, "ecircumflex"),
    (235, "edieresis"),
    (236, "igrave"),
    (237, "iacute"),
    (238, "icircumflex"),
    (239, "idieresis"),
    (240, "eth"),
    (241, "ntilde"),
    (242, "ograve"),
    (243, "oacute"),
    (244, "ocircumflex"),
    (245, "otilde"),
    (246, "odieresis"),
    (247, "divide"),
    (248, "oslash"),
    (249, "ugrave"),
    (250, "uacute"),
    (251, "ucircumflex"),
    (252, "udieresis"),
    (253, "yacute"),
    (254, "thorn"),
    (255, "ydieresis"),
];

/// PDF PDFDocEncoding table (Annex D.2).
pub static PDF_DOC_ENCODING: EncodingTable = build_pdf_doc();

const fn build_pdf_doc() -> EncodingTable {
    let mut table = build_ascii_based(false, &[]);
    let mut i = 0;
    while i < PDF_DOC_HIGH.len() {
        let (code, name) = PDF_DOC_HIGH[i];
        table[code as usize] = Some(name);
        i += 1;
    }
    table
}

// ---------------------------------------------------------------------------
// Symbol encoding (Adobe Symbol built-in, PDF Annex D.4)
// ---------------------------------------------------------------------------

/// Adobe Symbol font built-in encoding (PDF Annex D.4).
const SYMBOL_PAIRS: [(u8, &str); 189] = [
    (32, "space"),
    (33, "exclam"),
    (34, "universal"),
    (35, "numbersign"),
    (36, "existential"),
    (37, "percent"),
    (38, "ampersand"),
    (39, "suchthat"),
    (40, "parenleft"),
    (41, "parenright"),
    (42, "asteriskmath"),
    (43, "plus"),
    (44, "comma"),
    (45, "minus"),
    (46, "period"),
    (47, "slash"),
    (48, "zero"),
    (49, "one"),
    (50, "two"),
    (51, "three"),
    (52, "four"),
    (53, "five"),
    (54, "six"),
    (55, "seven"),
    (56, "eight"),
    (57, "nine"),
    (58, "colon"),
    (59, "semicolon"),
    (60, "less"),
    (61, "equal"),
    (62, "greater"),
    (63, "question"),
    (64, "congruent"),
    (65, "Alpha"),
    (66, "Beta"),
    (67, "Chi"),
    (68, "Delta"),
    (69, "Epsilon"),
    (70, "Phi"),
    (71, "Gamma"),
    (72, "Eta"),
    (73, "Iota"),
    (74, "theta1"),
    (75, "Kappa"),
    (76, "Lambda"),
    (77, "Mu"),
    (78, "Nu"),
    (79, "Omicron"),
    (80, "Pi"),
    (81, "Theta"),
    (82, "Rho"),
    (83, "Sigma"),
    (84, "Tau"),
    (85, "Upsilon"),
    (86, "sigma1"),
    (87, "Omega"),
    (88, "Xi"),
    (89, "Psi"),
    (90, "Zeta"),
    (91, "bracketleft"),
    (92, "therefore"),
    (93, "bracketright"),
    (94, "perpendicular"),
    (95, "underscore"),
    (96, "radicalex"),
    (97, "alpha"),
    (98, "beta"),
    (99, "chi"),
    (100, "delta"),
    (101, "epsilon"),
    (102, "phi"),
    (103, "gamma"),
    (104, "eta"),
    (105, "iota"),
    (106, "phi1"),
    (107, "kappa"),
    (108, "lambda"),
    (109, "mu"),
    (110, "nu"),
    (111, "omicron"),
    (112, "pi"),
    (113, "theta"),
    (114, "rho"),
    (115, "sigma"),
    (116, "tau"),
    (117, "upsilon"),
    (118, "omega1"),
    (119, "omega"),
    (120, "xi"),
    (121, "psi"),
    (122, "zeta"),
    (123, "braceleft"),
    (124, "bar"),
    (125, "braceright"),
    (126, "similar"),
    (160, "Euro"),
    (161, "Upsilon1"),
    (162, "minute"),
    (163, "lessequal"),
    (164, "fraction"),
    (165, "infinity"),
    (166, "florin"),
    (167, "club"),
    (168, "diamond"),
    (169, "heart"),
    (170, "spade"),
    (171, "arrowboth"),
    (172, "arrowleft"),
    (173, "arrowup"),
    (174, "arrowright"),
    (175, "arrowdown"),
    (176, "degree"),
    (177, "plusminus"),
    (178, "second"),
    (179, "greaterequal"),
    (180, "multiply"),
    (181, "proportional"),
    (182, "partialdiff"),
    (183, "bullet"),
    (184, "divide"),
    (185, "notequal"),
    (186, "equivalence"),
    (187, "approxequal"),
    (188, "ellipsis"),
    (189, "arrowvertex"),
    (190, "arrowhorizex"),
    (191, "carriagereturn"),
    (192, "aleph"),
    (193, "Ifraktur"),
    (194, "Rfraktur"),
    (195, "weierstrass"),
    (196, "circlemultiply"),
    (197, "circleplus"),
    (198, "emptyset"),
    (199, "intersection"),
    (200, "union"),
    (201, "propersuperset"),
    (202, "reflexsuperset"),
    (203, "notsubset"),
    (204, "propersubset"),
    (205, "reflexsubset"),
    (206, "element"),
    (207, "notelement"),
    (208, "angle"),
    (209, "gradient"),
    (210, "registerserif"),
    (211, "copyrightserif"),
    (212, "trademarkserif"),
    (213, "product"),
    (214, "radical"),
    (215, "dotmath"),
    (216, "logicalnot"),
    (217, "logicaland"),
    (218, "logicalor"),
    (219, "arrowdblboth"),
    (220, "arrowdblleft"),
    (221, "arrowdblup"),
    (222, "arrowdblright"),
    (223, "arrowdbldown"),
    (224, "lozenge"),
    (225, "angleleft"),
    (226, "registersans"),
    (227, "copyrightsans"),
    (228, "trademarksans"),
    (229, "summation"),
    (230, "parenlefttp"),
    (231, "parenleftex"),
    (232, "parenleftbt"),
    (233, "bracketlefttp"),
    (234, "bracketleftex"),
    (235, "bracketleftbt"),
    (236, "bracelefttp"),
    (237, "braceleftmid"),
    (238, "braceleftbt"),
    (239, "braceex"),
    (241, "angleright"),
    (242, "integral"),
    (243, "integraltp"),
    (244, "integralex"),
    (245, "integralbt"),
    (246, "parenrighttp"),
    (247, "parenrightex"),
    (248, "parenrightbt"),
    (249, "bracketrighttp"),
    (250, "bracketrightex"),
    (251, "bracketrightbt"),
    (252, "bracerighttp"),
    (253, "bracerightmid"),
    (254, "bracerightbt"),
];

/// Adobe Symbol font built-in encoding.
pub static SYMBOL_ENCODING: EncodingTable = build_from(&SYMBOL_PAIRS);

// ---------------------------------------------------------------------------
// ZapfDingbats encoding (Adobe ZapfDingbats built-in, PDF Annex D.5)
// ---------------------------------------------------------------------------

/// Adobe ITC ZapfDingbats font built-in encoding (PDF Annex D.5).
const ZAPF_PAIRS: [(u8, &str); 188] = [
    (32, "space"),
    (33, "a1"),
    (34, "a2"),
    (35, "a202"),
    (36, "a3"),
    (37, "a4"),
    (38, "a5"),
    (39, "a119"),
    (40, "a118"),
    (41, "a117"),
    (42, "a11"),
    (43, "a12"),
    (44, "a13"),
    (45, "a14"),
    (46, "a15"),
    (47, "a16"),
    (48, "a105"),
    (49, "a17"),
    (50, "a18"),
    (51, "a19"),
    (52, "a20"),
    (53, "a21"),
    (54, "a22"),
    (55, "a23"),
    (56, "a24"),
    (57, "a25"),
    (58, "a26"),
    (59, "a27"),
    (60, "a28"),
    (61, "a6"),
    (62, "a7"),
    (63, "a8"),
    (64, "a9"),
    (65, "a10"),
    (66, "a29"),
    (67, "a30"),
    (68, "a31"),
    (69, "a32"),
    (70, "a33"),
    (71, "a34"),
    (72, "a35"),
    (73, "a36"),
    (74, "a37"),
    (75, "a38"),
    (76, "a39"),
    (77, "a40"),
    (78, "a41"),
    (79, "a42"),
    (80, "a43"),
    (81, "a44"),
    (82, "a45"),
    (83, "a46"),
    (84, "a47"),
    (85, "a48"),
    (86, "a49"),
    (87, "a50"),
    (88, "a51"),
    (89, "a52"),
    (90, "a53"),
    (91, "a54"),
    (92, "a55"),
    (93, "a56"),
    (94, "a57"),
    (95, "a58"),
    (96, "a59"),
    (97, "a60"),
    (98, "a61"),
    (99, "a62"),
    (100, "a63"),
    (101, "a64"),
    (102, "a65"),
    (103, "a66"),
    (104, "a67"),
    (105, "a68"),
    (106, "a69"),
    (107, "a70"),
    (108, "a71"),
    (109, "a72"),
    (110, "a73"),
    (111, "a74"),
    (112, "a203"),
    (113, "a75"),
    (114, "a204"),
    (115, "a76"),
    (116, "a77"),
    (117, "a78"),
    (118, "a79"),
    (119, "a81"),
    (120, "a82"),
    (121, "a83"),
    (122, "a84"),
    (123, "a97"),
    (124, "a98"),
    (125, "a99"),
    (126, "a100"),
    (161, "a101"),
    (162, "a102"),
    (163, "a103"),
    (164, "a104"),
    (165, "a106"),
    (166, "a107"),
    (167, "a108"),
    (168, "a112"),
    (169, "a111"),
    (170, "a110"),
    (171, "a109"),
    (172, "a120"),
    (173, "a121"),
    (174, "a122"),
    (175, "a123"),
    (176, "a124"),
    (177, "a125"),
    (178, "a126"),
    (179, "a127"),
    (180, "a128"),
    (181, "a129"),
    (182, "a130"),
    (183, "a131"),
    (184, "a132"),
    (185, "a133"),
    (186, "a134"),
    (187, "a135"),
    (188, "a136"),
    (189, "a137"),
    (190, "a138"),
    (191, "a139"),
    (192, "a140"),
    (193, "a141"),
    (194, "a142"),
    (195, "a143"),
    (196, "a144"),
    (197, "a145"),
    (198, "a146"),
    (199, "a147"),
    (200, "a148"),
    (201, "a149"),
    (202, "a150"),
    (203, "a151"),
    (204, "a152"),
    (205, "a153"),
    (206, "a154"),
    (207, "a155"),
    (208, "a156"),
    (209, "a157"),
    (210, "a158"),
    (211, "a159"),
    (212, "a160"),
    (213, "a161"),
    (214, "a163"),
    (215, "a164"),
    (216, "a196"),
    (217, "a165"),
    (218, "a192"),
    (219, "a166"),
    (220, "a167"),
    (221, "a168"),
    (222, "a169"),
    (223, "a170"),
    (224, "a171"),
    (225, "a172"),
    (226, "a173"),
    (227, "a162"),
    (228, "a174"),
    (229, "a175"),
    (230, "a176"),
    (231, "a177"),
    (232, "a178"),
    (233, "a179"),
    (234, "a193"),
    (235, "a180"),
    (236, "a199"),
    (237, "a181"),
    (238, "a200"),
    (239, "a182"),
    (241, "a201"),
    (242, "a183"),
    (243, "a184"),
    (244, "a197"),
    (245, "a185"),
    (246, "a194"),
    (247, "a198"),
    (248, "a186"),
    (249, "a195"),
    (250, "a187"),
    (251, "a188"),
    (252, "a189"),
    (253, "a190"),
    (254, "a191"),
];

/// Adobe ITC ZapfDingbats font built-in encoding.
pub static ZAPF_DINGBATS_ENCODING: EncodingTable = build_from(&ZAPF_PAIRS);

// ---------------------------------------------------------------------------
// Name lookup
// ---------------------------------------------------------------------------

/// Look up a base encoding table by its PDF name.
///
/// Recognizes `StandardEncoding`, `WinAnsiEncoding`, `MacRomanEncoding`,
/// `PDFDocEncoding`, `Symbol`, and `ZapfDingbats`.
pub fn base_encoding_by_name(name: &str) -> Option<&'static EncodingTable> {
    match name {
        "StandardEncoding" => Some(&STANDARD_ENCODING),
        "WinAnsiEncoding" => Some(&WIN_ANSI_ENCODING),
        "MacRomanEncoding" => Some(&MAC_ROMAN_ENCODING),
        "PDFDocEncoding" => Some(&PDF_DOC_ENCODING),
        "Symbol" => Some(&SYMBOL_ENCODING),
        "ZapfDingbats" => Some(&ZAPF_DINGBATS_ENCODING),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Effective per-font Encoding (base + /Differences)
// ---------------------------------------------------------------------------

/// An effective per-font encoding: a base table with optional `/Differences`
/// overrides applied. Entries borrow from a static base where possible and
/// own (heap-allocate) only the glyph names introduced by `/Differences`.
#[derive(Debug, Clone)]
pub struct Encoding {
    table: Box<[Option<Cow<'static, str>>; 256]>,
}

impl Encoding {
    /// Build an effective encoding from a static base table.
    pub fn from_base(table: &'static EncodingTable) -> Self {
        let arr: [Option<Cow<'static, str>>; 256] =
            std::array::from_fn(|i| table[i].map(Cow::Borrowed));
        Encoding {
            table: Box::new(arr),
        }
    }

    /// StandardEncoding base.
    pub fn standard() -> Self {
        Self::from_base(&STANDARD_ENCODING)
    }

    /// WinAnsiEncoding base.
    pub fn win_ansi() -> Self {
        Self::from_base(&WIN_ANSI_ENCODING)
    }

    /// An encoding where every code is undefined.
    pub fn empty() -> Self {
        let arr: [Option<Cow<'static, str>>; 256] = std::array::from_fn(|_| None);
        Encoding {
            table: Box::new(arr),
        }
    }

    /// Apply a single `/Differences` override, replacing the glyph at `code`.
    /// The supplied name is owned (heap-allocated) so any `&str` is accepted.
    pub fn apply_difference(&mut self, code: u8, name: &str) {
        self.table[code as usize] = Some(Cow::Owned(name.to_owned()));
    }

    /// Return the glyph name mapped to `code`, or `None` if undefined.
    pub fn glyph_name(&self, code: u8) -> Option<&str> {
        self.table[code as usize].as_deref()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winansi_basic_ascii() {
        assert_eq!(WIN_ANSI_ENCODING[65], Some("A"));
        assert_eq!(WIN_ANSI_ENCODING[97], Some("a"));
        assert_eq!(WIN_ANSI_ENCODING[32], Some("space"));
        assert_eq!(WIN_ANSI_ENCODING[48], Some("zero"));
    }

    #[test]
    fn winansi_quotes() {
        assert_eq!(WIN_ANSI_ENCODING[39], Some("quotesingle"));
        assert_eq!(WIN_ANSI_ENCODING[96], Some("grave"));
    }

    #[test]
    fn winansi_high_range() {
        assert_eq!(WIN_ANSI_ENCODING[0x80], Some("Euro"));
        assert_eq!(WIN_ANSI_ENCODING[146], Some("quoteright"));
        assert_eq!(WIN_ANSI_ENCODING[145], Some("quoteleft"));
        assert_eq!(WIN_ANSI_ENCODING[150], Some("endash"));
        assert_eq!(WIN_ANSI_ENCODING[151], Some("emdash"));
        assert_eq!(WIN_ANSI_ENCODING[160], Some("space"));
        assert_eq!(WIN_ANSI_ENCODING[169], Some("copyright"));
        assert_eq!(WIN_ANSI_ENCODING[192], Some("Agrave"));
        assert_eq!(WIN_ANSI_ENCODING[247], Some("divide"));
        assert_eq!(WIN_ANSI_ENCODING[255], Some("ydieresis"));
        // Undefined code in Windows-1252.
        assert_eq!(WIN_ANSI_ENCODING[129], None);
    }

    #[test]
    fn standard_quotes_differ() {
        assert_eq!(STANDARD_ENCODING[39], Some("quoteright"));
        assert_eq!(STANDARD_ENCODING[96], Some("quoteleft"));
        // Shared ASCII letters stay identical.
        assert_eq!(STANDARD_ENCODING[65], Some("A"));
    }

    #[test]
    fn standard_high_range() {
        assert_eq!(STANDARD_ENCODING[161], Some("exclamdown"));
        assert_eq!(STANDARD_ENCODING[164], Some("fraction"));
        assert_eq!(STANDARD_ENCODING[166], Some("florin"));
        assert_eq!(STANDARD_ENCODING[174], Some("fi"));
        assert_eq!(STANDARD_ENCODING[177], Some("endash"));
        assert_eq!(STANDARD_ENCODING[208], Some("emdash"));
        assert_eq!(STANDARD_ENCODING[225], Some("AE"));
        assert_eq!(STANDARD_ENCODING[251], Some("germandbls"));
        // The grave accent lives at 193 (octal 0o301), not at 192.
        assert_eq!(STANDARD_ENCODING[193], Some("grave"));
        // No Latin-1 accented letters in StandardEncoding; 192 and 255 unused.
        assert_eq!(STANDARD_ENCODING[192], None);
        assert_eq!(STANDARD_ENCODING[255], None);
    }

    #[test]
    fn macroman_high_range() {
        assert_eq!(MAC_ROMAN_ENCODING[128], Some("Adieresis"));
        assert_eq!(MAC_ROMAN_ENCODING[165], Some("bullet"));
        assert_eq!(MAC_ROMAN_ENCODING[202], Some("space"));
        assert_eq!(MAC_ROMAN_ENCODING[208], Some("endash"));
        assert_eq!(MAC_ROMAN_ENCODING[209], Some("emdash"));
        assert_eq!(MAC_ROMAN_ENCODING[210], Some("quotedblleft"));
        assert_eq!(MAC_ROMAN_ENCODING[213], Some("quoteright"));
        assert_eq!(MAC_ROMAN_ENCODING[214], Some("divide"));
        assert_eq!(MAC_ROMAN_ENCODING[222], Some("fi"));
        assert_eq!(MAC_ROMAN_ENCODING[223], Some("fl"));
        assert_eq!(MAC_ROMAN_ENCODING[228], Some("perthousand"));
        // ASCII still works.
        assert_eq!(MAC_ROMAN_ENCODING[65], Some("A"));
    }

    #[test]
    fn pdfdoc_basics() {
        assert_eq!(PDF_DOC_ENCODING[65], Some("A"));
        assert_eq!(PDF_DOC_ENCODING[39], Some("quotesingle"));
        assert_eq!(PDF_DOC_ENCODING[150], Some("OE"));
        assert_eq!(PDF_DOC_ENCODING[183], Some("periodcentered"));
        assert_eq!(PDF_DOC_ENCODING[192], Some("Agrave"));
        assert_eq!(PDF_DOC_ENCODING[255], Some("ydieresis"));
    }

    #[test]
    fn symbol_basics() {
        assert_eq!(SYMBOL_ENCODING[32], Some("space"));
        assert_eq!(SYMBOL_ENCODING[65], Some("Alpha"));
        assert_eq!(SYMBOL_ENCODING[66], Some("Beta"));
        assert_eq!(SYMBOL_ENCODING[97], Some("alpha"));
        assert_eq!(SYMBOL_ENCODING[112], Some("pi"));
        assert_eq!(SYMBOL_ENCODING[183], Some("bullet"));
        // No ASCII letters as Latin glyphs in Symbol.
        assert_ne!(SYMBOL_ENCODING[65], Some("A"));
    }

    #[test]
    fn zapf_basics() {
        assert_eq!(ZAPF_DINGBATS_ENCODING[32], Some("space"));
        assert_eq!(ZAPF_DINGBATS_ENCODING[33], Some("a1"));
        assert_eq!(ZAPF_DINGBATS_ENCODING[126], Some("a100"));
        assert_eq!(ZAPF_DINGBATS_ENCODING[161], Some("a101"));
        assert_eq!(ZAPF_DINGBATS_ENCODING[254], Some("a191"));
    }

    #[test]
    fn name_lookup() {
        assert!(base_encoding_by_name("WinAnsiEncoding").is_some());
        assert!(base_encoding_by_name("StandardEncoding").is_some());
        assert!(base_encoding_by_name("MacRomanEncoding").is_some());
        assert!(base_encoding_by_name("PDFDocEncoding").is_some());
        assert!(base_encoding_by_name("Symbol").is_some());
        assert!(base_encoding_by_name("ZapfDingbats").is_some());
        assert!(base_encoding_by_name("NoSuchEncoding").is_none());
        // The returned table is the genuine static.
        let t = base_encoding_by_name("WinAnsiEncoding").unwrap();
        assert_eq!(t[0x80], Some("Euro"));
    }

    #[test]
    fn encoding_from_base_and_glyph_name() {
        let enc = Encoding::win_ansi();
        assert_eq!(enc.glyph_name(65), Some("A"));
        assert_eq!(enc.glyph_name(0x80), Some("Euro"));
        assert_eq!(enc.glyph_name(129), None);

        let std = Encoding::standard();
        assert_eq!(std.glyph_name(39), Some("quoteright"));
    }

    #[test]
    fn encoding_differences() {
        let mut enc = Encoding::win_ansi();
        enc.apply_difference(65, "Alpha");
        assert_eq!(enc.glyph_name(65), Some("Alpha"));
        // Unchanged codes remain.
        assert_eq!(enc.glyph_name(66), Some("B"));
        // Differences can introduce a glyph at a previously-undefined code.
        enc.apply_difference(129, "myglyph");
        assert_eq!(enc.glyph_name(129), Some("myglyph"));
    }

    #[test]
    fn encoding_empty() {
        let enc = Encoding::empty();
        for code in 0u16..=255 {
            assert_eq!(enc.glyph_name(code as u8), None);
        }
    }

    #[test]
    fn from_base_borrows_static() {
        // A from_base encoding should reflect the static table exactly.
        let enc = Encoding::from_base(&MAC_ROMAN_ENCODING);
        for code in 0u16..=255 {
            assert_eq!(
                enc.glyph_name(code as u8),
                MAC_ROMAN_ENCODING[code as usize]
            );
        }
    }
}
