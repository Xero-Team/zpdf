#![no_main]
//! Fuzz the stream filter pipeline: `zpdf_parser::filters::decode_stream`. The
//! first bytes of the input are consumed as a small configuration header that
//! selects the filter (and, for the compression filters, an optional predictor
//! configuration); the remainder is the raw stream payload. This lets the
//! fuzzer explore every codec — Flate / LZW / ASCIIHex / ASCII85 / RunLength /
//! CCITTFax / JBIG2 / DCT — plus the PNG/TIFF predictor post-pass and its
//! overflow guards. Invariant: never panic, never hang, never over-allocate
//! past the decoder's internal caps.

use libfuzzer_sys::fuzz_target;
use zpdf_core::{PdfDict, PdfName, PdfObject};

/// The filters `decode_stream` dispatches on. Includes the abbreviated inline
/// names so both spellings are covered.
const FILTERS: &[&str] = &[
    "FlateDecode",
    "Fl",
    "LZWDecode",
    "LZW",
    "ASCIIHexDecode",
    "AHx",
    "ASCII85Decode",
    "A85",
    "RunLengthDecode",
    "RL",
    "CCITTFaxDecode",
    "CCF",
    "JBIG2Decode",
    "DCTDecode",
    "JPXDecode",
];

fuzz_target!(|data: &[u8]| {
    // Need at least a 2-byte config header; below that there is nothing to do.
    let (&sel, rest) = match data.split_first() {
        Some(x) => x,
        None => return,
    };
    let (&parms_sel, payload) = match rest.split_first() {
        Some(x) => x,
        None => (&0u8, rest),
    };

    let filter = FILTERS[sel as usize % FILTERS.len()];

    let mut dict = PdfDict::new();
    dict.insert(
        PdfName::new("Filter"),
        PdfObject::Name(PdfName::new(filter)),
    );

    // For the compression filters, sometimes attach a DecodeParms with a
    // predictor + geometry so the predictor overflow guards get hammered. The
    // low 3 bits pick a predictor mode; the next bits pick column/color/bpc
    // combos including deliberately awkward ones.
    if matches!(filter, "FlateDecode" | "Fl" | "LZWDecode" | "LZW") && parms_sel & 0x80 != 0 {
        let predictor = match parms_sel & 0b11 {
            0 => 1,
            1 => 2,  // TIFF
            2 => 12, // PNG Up
            _ => 15, // PNG optimum
        };
        let columns = match (parms_sel >> 2) & 0b11 {
            0 => 1,
            1 => 8,
            2 => 256,
            _ => 65536, // at the MAX_COLUMNS clamp boundary
        };
        let colors = match (parms_sel >> 4) & 0b1 {
            0 => 1,
            _ => 4,
        };
        let bpc = match (parms_sel >> 5) & 0b11 {
            0 => 1,
            1 => 8,
            2 => 16,
            _ => 32, // at the MAX_BPC clamp boundary
        };

        let mut parms = PdfDict::new();
        parms.insert(PdfName::new("Predictor"), PdfObject::Integer(predictor));
        parms.insert(PdfName::new("Columns"), PdfObject::Integer(columns));
        parms.insert(PdfName::new("Colors"), PdfObject::Integer(colors));
        parms.insert(
            PdfName::new("BitsPerComponent"),
            PdfObject::Integer(bpc),
        );
        dict.insert(PdfName::new("DecodeParms"), PdfObject::Dict(parms));
    }

    // For CCITT, feed a couple of geometry knobs so the G3/G4 decoder explores
    // more than its defaults.
    if matches!(filter, "CCITTFaxDecode" | "CCF") {
        let mut parms = PdfDict::new();
        parms.insert(
            PdfName::new("K"),
            PdfObject::Integer(match parms_sel & 0b11 {
                0 => 0,
                1 => -1,
                _ => 1,
            }),
        );
        parms.insert(PdfName::new("Columns"), PdfObject::Integer(1728));
        dict.insert(PdfName::new("DecodeParms"), PdfObject::Dict(parms));
    }

    let _ = zpdf_parser::filters::decode_stream(payload, &dict);
});
