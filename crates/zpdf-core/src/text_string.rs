//! PDF text strings (ISO 32000-1 7.9.2.2/Annex D.2 and PDF 2.0 UTF-8).
//! Content-stream font codes and binary strings use their own encodings.
use crate::PdfString;

const ACCENTS: [char; 8] = [
    '\u{02d8}', '\u{02c7}', '\u{02c6}', '\u{02d9}', '\u{02dd}', '\u{02db}', '\u{02da}', '\u{02dc}',
];
const SPECIAL: [char; 31] = [
    '\u{2022}', '\u{2020}', '\u{2021}', '\u{2026}', '\u{2014}', '\u{2013}', '\u{0192}', '\u{2044}',
    '\u{2039}', '\u{203a}', '\u{2212}', '\u{2030}', '\u{201e}', '\u{201c}', '\u{201d}', '\u{2018}',
    '\u{2019}', '\u{201a}', '\u{2122}', '\u{fb01}', '\u{fb02}', '\u{0141}', '\u{0152}', '\u{0160}',
    '\u{0178}', '\u{017d}', '\u{0131}', '\u{0142}', '\u{0153}', '\u{0161}', '\u{017e}',
];

fn pdf_doc_char(byte: u8) -> char {
    match byte {
        0x18..=0x1f => ACCENTS[(byte - 0x18) as usize],
        0x80..=0x9e => SPECIAL[(byte - 0x80) as usize],
        0xa0 => '\u{20ac}',
        0x7f | 0x9f | 0xad => char::REPLACEMENT_CHARACTER,
        _ => char::from(byte),
    }
}

/// Decode a PDF text string. BOMs select UTF-16BE or PDF 2.0 UTF-8;
/// otherwise use PDFDocEncoding. Undefined bytes and malformed Unicode are
/// replaced with U+FFFD, including an incomplete final UTF-16 code unit.
pub fn decode_text_string(bytes: &[u8]) -> String {
    if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let (pairs, tail) = bytes.as_chunks::<2>();
        let mut text: String = char::decode_utf16(pairs.iter().map(|p| u16::from_be_bytes(*p)))
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
        if !tail.is_empty() {
            text.push(char::REPLACEMENT_CHARACTER);
        }
        text
    } else if let Some(bytes) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        bytes.iter().map(|&b| pdf_doc_char(b)).collect()
    }
}

fn pdf_doc_byte(ch: char) -> Option<u8> {
    if (ch as u32) <= 255 && ch != char::REPLACEMENT_CHARACTER {
        let byte = ch as u8;
        if pdf_doc_char(byte) == ch {
            return Some(byte);
        }
    }
    if ch == '\u{20ac}' {
        return Some(0xa0);
    }
    ACCENTS
        .iter()
        .position(|&c| c == ch)
        .map(|i| i as u8 + 0x18)
        .or_else(|| {
            SPECIAL
                .iter()
                .position(|&c| c == ch)
                .map(|i| i as u8 + 0x80)
        })
}

/// Encode losslessly using PDFDocEncoding when possible, otherwise UTF-16BE
/// with a BOM (compatible with pre-2.0 PDFs). Avoid ambiguous leading BOM bytes.
pub fn encode_text_string(text: &str) -> PdfString {
    if let Some(bytes) = text.chars().map(pdf_doc_byte).collect::<Option<Vec<_>>>() {
        if !bytes.starts_with(&[0xfe, 0xff]) && !bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            return PdfString(bytes);
        }
    }
    let mut bytes = vec![0xfe, 0xff];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    PdfString(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pdfdocencoding_specials_and_undefined_bytes() {
        assert_eq!(
            decode_text_string(&[0x80, 0x8a, 0x93, 0xa0, 0x18, 0xe9]),
            "•−ﬁ€˘é"
        );
        assert_eq!(decode_text_string(&[0x7f, 0x9f, 0xad]), "���");
        for byte in 0..=255u8 {
            if !matches!(byte, 0x7f | 0x9f | 0xad) {
                assert_eq!(
                    encode_text_string(&decode_text_string(&[byte])).as_bytes(),
                    &[byte]
                );
            }
        }
    }
    #[test]
    fn unicode_boms_and_malformed_sequences() {
        assert_eq!(
            decode_text_string(b"\xef\xbb\xbf\xe4\xb8\xad\xf0\x9f\x98\x80"),
            "中😀"
        );
        assert_eq!(
            decode_text_string(&[0xfe, 0xff, 0xd8, 0x3d, 0xde, 0x00]),
            "😀"
        );
        assert_eq!(decode_text_string(&[0xfe, 0xff, 0xd8, 0x00, 0x41]), "��");
        assert_eq!(decode_text_string(b"\xef\xbb\xbf\xff"), "�");
        for s in ["中😀", "\u{00a0}\u{00ad}\u{0080}", "þÿ", "ï»¿", "•€ﬁ", ""] {
            assert_eq!(decode_text_string(encode_text_string(s).as_bytes()), s);
        }
    }
}
