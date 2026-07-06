//! Incremental PDF writer for appending new content (annotations, etc.) to
//! existing PDFs without rewriting the entire file.
//!
//! This module implements the **incremental update** mechanism defined in ISO
//! 32000-1 §7.5.6. An incremental update appends:
//! 1. New indirect objects (e.g., annotations, appearance streams)
//! 2. Modified objects (e.g., a page dict with an updated `/Annots` array)
//! 3. A new cross-reference section covering only the new/modified objects
//! 4. A new trailer with `/Prev` pointing to the original xref position
//!
//! The original file content is left untouched, making incremental updates safe
//! (no risk of corruption if the write fails partway) and efficient (no need to
//! parse or re-serialize the entire object graph).

use std::io::{self, Seek, Write};
use std::sync::Arc;
use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Result};
use zpdf_document::{InkAnnotDict, PdfDocument};
use zpdf_parser::PdfFile;

mod serialize;
use serialize::{serialize_object, serialize_stream};

/// An incremental PDF writer. Accumulates new objects and writes them as an
/// incremental update appended to the original file.
pub struct IncrementalWriter {
    /// The original PDF bytes (copied verbatim to the output).
    original: Vec<u8>,
    /// The next free object number (starts at original `/Size`).
    next_obj_num: u32,
    /// New objects to append: `(obj_num, gen_num, serialized_bytes)`.
    new_objects: Vec<(u32, u32, Vec<u8>)>,
    /// The original xref position (for `/Prev` in the new trailer).
    original_xref_pos: u64,
    /// The catalog reference (for the new trailer `/Root`).
    catalog_ref: (u32, u32),
}

impl IncrementalWriter {
    /// Create a new incremental writer from the original PDF bytes. Parses just
    /// enough to locate the trailer and determine the starting object number.
    pub fn new(original: Vec<u8>) -> Result<Self> {
        let original_arc: Arc<[u8]> = original.clone().into();
        let file = PdfFile::parse(original_arc)?;
        let size = file
            .trailer
            .get("Size")
            .and_then(|o| match o {
                PdfObject::Integer(n) => Some(*n as u32),
                _ => None,
            })
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "trailer missing /Size")
            })?;

        let root = file.trailer.get_ref("Root").map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "trailer missing /Root")
        })?;

        // The original xref position is wherever `startxref` pointed. Scan backward
        // from the end to find the "startxref\n<number>" line.
        let original_xref_pos = find_startxref(&original).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "could not find startxref in original PDF",
            )
        })?;

        Ok(Self {
            original,
            next_obj_num: size,
            new_objects: Vec::new(),
            original_xref_pos,
            catalog_ref: (root.0, root.1 as u32),
        })
    }

    /// Add a new indirect object. Returns its `(obj_num, gen_num)`. Generation
    /// is always 0 for newly created objects.
    pub fn add_object(&mut self, obj: &PdfObject) -> (u32, u32) {
        let num = self.next_obj_num;
        self.next_obj_num += 1;
        let serialized = serialize_object(num, 0, obj);
        self.new_objects.push((num, 0, serialized));
        (num, 0)
    }

    /// Add a stream object (a dictionary plus binary data). Returns its object reference.
    pub fn add_stream(&mut self, dict: &PdfDict, data: &[u8]) -> (u32, u32) {
        let num = self.next_obj_num;
        self.next_obj_num += 1;
        let serialized = serialize_stream(num, 0, dict, data);
        self.new_objects.push((num, 0, serialized));
        (num, 0)
    }

    /// Append an ink annotation to the specified page. This:
    /// 1. Adds the appearance XObject as a new stream
    /// 2. Adds the annotation dict (with `/AP /N` referencing the XObject) as a new object
    /// 3. Adds a *modified* page dict with the annotation appended to `/Annots`
    ///
    /// The page is identified by its 0-based index.
    pub fn add_ink_annotation_to_page(
        &mut self,
        page_index: usize,
        annot_dict: &InkAnnotDict,
        appearance_stream: &[u8],
    ) -> Result<()> {
        // Re-parse to access the page tree (we need the page's object ID and existing /Annots).
        let original_arc: Arc<[u8]> = self.original.clone().into();
        let doc = PdfDocument::open(original_arc.clone())?;
        let page = doc.page(page_index).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("page {} not found", page_index),
            )
        })?;
        let page_obj_id = page.id;

        // 1. Add the appearance stream (a Form XObject).
        let appearance_dict = self.build_appearance_dict(annot_dict);
        let ap_ref = self.add_stream(&appearance_dict, appearance_stream);

        // 2. Add the annotation dict.
        let annot_pdf_dict = self.build_annot_dict(annot_dict, ap_ref);
        let annot_ref = self.add_object(&PdfObject::Dict(annot_pdf_dict));

        // 3. Add a modified page dict with the annotation appended to /Annots.
        self.add_modified_page_dict(page_obj_id, annot_ref)?;

        Ok(())
    }

    /// Build the appearance stream dictionary (Form XObject).
    fn build_appearance_dict(&self, annot: &InkAnnotDict) -> PdfDict {
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName("Type".to_string()),
            PdfObject::Name(PdfName("XObject".to_string())),
        );
        dict.insert(
            PdfName("Subtype".to_string()),
            PdfObject::Name(PdfName("Form".to_string())),
        );
        dict.insert(PdfName("FormType".to_string()), PdfObject::Integer(1));
        dict.insert(
            PdfName("BBox".to_string()),
            PdfObject::Array(vec![
                PdfObject::Real(annot.rect.x0),
                PdfObject::Real(annot.rect.y0),
                PdfObject::Real(annot.rect.x1),
                PdfObject::Real(annot.rect.y1),
            ]),
        );
        // We could set /Matrix here if needed, but the default [1 0 0 1 0 0] is fine.
        dict
    }

    /// Build the annotation dictionary with all required fields.
    fn build_annot_dict(&self, annot: &InkAnnotDict, ap_ref: (u32, u32)) -> PdfDict {
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName("Type".to_string()),
            PdfObject::Name(PdfName("Annot".to_string())),
        );
        dict.insert(
            PdfName("Subtype".to_string()),
            PdfObject::Name(PdfName("Ink".to_string())),
        );
        dict.insert(
            PdfName("Rect".to_string()),
            PdfObject::Array(vec![
                PdfObject::Real(annot.rect.x0),
                PdfObject::Real(annot.rect.y0),
                PdfObject::Real(annot.rect.x1),
                PdfObject::Real(annot.rect.y1),
            ]),
        );

        // /InkList: array of arrays of numbers.
        let ink_list: Vec<PdfObject> = annot
            .ink_list
            .iter()
            .map(|stroke| {
                let coords: Vec<PdfObject> = stroke
                    .iter()
                    .flat_map(|&(x, y)| vec![PdfObject::Real(x), PdfObject::Real(y)])
                    .collect();
                PdfObject::Array(coords)
            })
            .collect();
        dict.insert(PdfName("InkList".to_string()), PdfObject::Array(ink_list));

        // /C: color (DeviceRGB).
        let (r, g, b) = annot.color;
        dict.insert(
            PdfName("C".to_string()),
            PdfObject::Array(vec![
                PdfObject::Real(r),
                PdfObject::Real(g),
                PdfObject::Real(b),
            ]),
        );

        // /BS: border style.
        let mut bs = PdfDict::new();
        bs.insert(PdfName("W".to_string()), PdfObject::Real(annot.width));
        dict.insert(PdfName("BS".to_string()), PdfObject::Dict(bs));

        // /AP: appearance dictionary with /N pointing to the XObject.
        let mut ap = PdfDict::new();
        ap.insert(
            PdfName("N".to_string()),
            PdfObject::Ref(ObjectId(ap_ref.0, ap_ref.1 as u16)),
        );
        dict.insert(PdfName("AP".to_string()), PdfObject::Dict(ap));

        dict
    }

    /// Add a modified version of the page dict with the new annotation appended
    /// to its `/Annots` array.
    fn add_modified_page_dict(
        &mut self,
        page_obj_id: ObjectId,
        annot_ref: (u32, u32),
    ) -> Result<()> {
        // Re-resolve the page dict from the original file.
        let original_arc: Arc<[u8]> = self.original.clone().into();
        let file = PdfFile::parse(original_arc)?;
        let page_obj = file.resolve(page_obj_id)?;
        let mut page_dict = page_obj.as_dict()?.clone();

        // Get the existing /Annots array (or create a new one).
        let annots = match page_dict.get("Annots") {
            Some(PdfObject::Ref(r)) => {
                // Follow the reference.
                match file.resolve(*r) {
                    Ok(obj) => obj.as_array().ok().map(|a| a.to_vec()).unwrap_or_default(),
                    Err(_) => Vec::new(),
                }
            }
            Some(PdfObject::Array(arr)) => arr.to_vec(),
            _ => Vec::new(),
        };

        // Append the new annotation reference.
        let mut new_annots = annots;
        new_annots.push(PdfObject::Ref(ObjectId(annot_ref.0, annot_ref.1 as u16)));
        page_dict.insert(PdfName("Annots".to_string()), PdfObject::Array(new_annots));

        // Add the modified page dict with the **same object number** as the original.
        let (num, gen) = (page_obj_id.0, page_obj_id.1 as u32);
        let serialized = serialize_object(num, gen, &PdfObject::Dict(page_dict));
        self.new_objects.push((num, gen, serialized));

        Ok(())
    }

    /// Write the incremental update to the output. The output must be seekable
    /// (to record xref offsets).
    pub fn write<W: Write + Seek>(&self, mut out: W) -> io::Result<()> {
        // 1. Copy the original PDF.
        out.write_all(&self.original)?;

        // 2. Append new objects, recording their offsets.
        let mut xref_entries = Vec::new();
        for (num, gen, bytes) in &self.new_objects {
            let offset = out.stream_position()?;
            xref_entries.push((*num, *gen, offset));
            out.write_all(bytes)?;
        }

        // 3. Append the new xref section.
        let xref_pos = out.stream_position()?;
        self.write_xref(&mut out, &xref_entries)?;

        // 4. Append the new trailer.
        self.write_trailer(&mut out, xref_pos)?;

        Ok(())
    }

    /// Write the xref section for the new objects.
    fn write_xref<W: Write>(&self, out: &mut W, entries: &[(u32, u32, u64)]) -> io::Result<()> {
        // Group entries into contiguous runs (subsections).
        let mut runs: Vec<Vec<(u32, u32, u64)>> = Vec::new();
        for &entry in entries {
            if let Some(last_run) = runs.last_mut() {
                let last_num = last_run.last().unwrap().0;
                if entry.0 == last_num + 1 {
                    last_run.push(entry);
                    continue;
                }
            }
            runs.push(vec![entry]);
        }

        writeln!(out, "xref")?;
        for run in runs {
            let first = run[0].0;
            let count = run.len();
            writeln!(out, "{} {}", first, count)?;
            for (_, gen, offset) in run {
                writeln!(out, "{:010} {:05} n ", offset, gen)?;
            }
        }
        Ok(())
    }

    /// Write the new trailer.
    fn write_trailer<W: Write>(&self, out: &mut W, xref_pos: u64) -> io::Result<()> {
        writeln!(out, "trailer")?;
        write!(
            out,
            "<< /Size {} /Prev {} /Root {} {} R >>",
            self.next_obj_num, self.original_xref_pos, self.catalog_ref.0, self.catalog_ref.1
        )?;
        writeln!(out)?;
        writeln!(out, "startxref")?;
        writeln!(out, "{}", xref_pos)?;
        writeln!(out, "%%EOF")?;
        Ok(())
    }
}

/// Find the `startxref` offset in a PDF file by scanning backward from the end.
fn find_startxref(data: &[u8]) -> Option<u64> {
    let tail = &data[data.len().saturating_sub(512)..];
    let s = String::from_utf8_lossy(tail);
    s.rfind("startxref").and_then(|pos| {
        let after = &s[pos + "startxref".len()..];
        // The number is on the next non-empty line after "startxref"
        after
            .lines()
            .nth(1) // Skip the empty line immediately after "startxref"
            .and_then(|line| line.trim().parse::<u64>().ok())
    })
}
