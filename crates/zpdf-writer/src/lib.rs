//! Incremental PDF writer for appending new content (annotations, form values,
//! page edits, metadata, stamps) to existing PDFs without rewriting the file.
//!
//! This module implements the **incremental update** mechanism defined in ISO
//! 32000-1 §7.5.6. An incremental update appends:
//! 1. New indirect objects (e.g., annotations, appearance streams)
//! 2. Modified objects (e.g., a page dict with an updated `/Annots` array)
//! 3. A new cross-reference section covering only the new/modified objects —
//!    a classic xref table, or a cross-reference *stream* when the original
//!    file uses one (§7.5.8 requires updates to match)
//! 4. A new trailer with `/Prev` pointing to the original xref position
//!
//! The original file content is left untouched, making incremental updates safe
//! (no risk of corruption if the write fails partway) and efficient (no need to
//! parse or re-serialize the entire object graph).
//!
//! Edits **compose**: every modification reads the latest pending version of an
//! object via [`IncrementalWriter::resolve_current`], so e.g. stamping and
//! rotating the same page in one update works. Objects are serialized once, at
//! [`IncrementalWriter::write`] time.
//!
//! # Limitations
//!
//! - Encrypted documents are refused ([`IncrementalWriter::new`] errors):
//!   new objects would have to be encrypted with the document's key, which is
//!   not yet supported.
//! - Objects orphaned by an edit (e.g. deleted pages) are left in place, not
//!   added to the free list — harmless for incremental updates.

use std::collections::BTreeMap;
use std::io::{self, Seek, Write};
use std::sync::Arc;
use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Result};
use zpdf_document::{InkAnnotDict, PdfDocument};

mod serialize;
use serialize::{serialize_dict, write_object, write_stream};

pub mod forms;
pub mod metadata;
pub mod pages;
pub mod stamp;

pub use forms::FormFiller;
pub use metadata::InfoUpdate;
pub use stamp::{jpeg_dimensions, StampImage, StampItem};

/// An object queued for the incremental update, kept unserialized so later
/// edits can still read/replace it ("last write wins").
enum PendingObject {
    Object(PdfObject),
    /// Stream dict + (possibly already compressed) data.
    Stream(PdfDict, Arc<[u8]>),
}

/// The cross-reference flavor of the original file, which the update must
/// match (ISO 32000-1 §7.5.8): classic `xref` table or cross-reference stream.
enum XrefKind {
    Table,
    Stream,
}

/// An incremental PDF writer. Accumulates new and modified objects and writes
/// them as an incremental update appended to the original file.
pub struct IncrementalWriter {
    /// The parsed original document (its raw bytes are copied verbatim to the
    /// output; its object store backs [`Self::resolve_current`]).
    doc: PdfDocument,
    /// The next free object number (starts at original `/Size`).
    next_obj_num: u32,
    /// Pending objects keyed by object number — overwriting the same object
    /// twice keeps only the latest version (single xref entry).
    pending: BTreeMap<u32, (u16, PendingObject)>,
    /// The original xref position (for `/Prev` in the new trailer).
    original_xref_pos: u64,
    /// The catalog reference (for the new trailer `/Root`).
    catalog_ref: ObjectId,
    /// Whether the original uses a classic xref table or an xref stream.
    xref_kind: XrefKind,
    /// Set when `/Info` was created (rather than overwritten in place) and the
    /// new trailer must point at the new object.
    info_ref_override: Option<ObjectId>,
}

impl IncrementalWriter {
    /// Create a new incremental writer from the original PDF bytes. Parses the
    /// document once; all subsequent edits resolve objects through it.
    ///
    /// Errors when the document is encrypted (updating an encrypted file would
    /// require encrypting every new string/stream, which is not supported yet).
    pub fn new(original: Vec<u8>) -> Result<Self> {
        let original_arc: Arc<[u8]> = original.into();
        let doc = PdfDocument::open(original_arc)?;
        if doc.is_encrypted() {
            return Err(unsupported("cannot incrementally update an encrypted document").into());
        }
        let file = doc.file();
        let trailer_size = file.trailer.get("Size").and_then(|o| match o {
            PdfObject::Integer(n) => u32::try_from(*n).ok(),
            _ => None,
        });
        // A malformed or stale /Size must not make newly-added objects collide
        // with existing ones. Derive the authoritative lower bound from the
        // parsed xref and take the larger value.
        let known_next = file
            .all_object_ids()
            .into_iter()
            .try_fold(0u32, |next, id| {
                id.0.checked_add(1).map(|candidate| next.max(candidate))
            })
            .ok_or_else(|| invalid_data("object number space is exhausted"))?;
        let size = trailer_size.unwrap_or(known_next).max(known_next).max(1);
        if size == u32::MAX {
            return Err(invalid_data("object number space is exhausted").into());
        }

        let root = file
            .trailer
            .get_ref("Root")
            .map_err(|_| invalid_data("trailer missing /Root"))?;

        // The original xref position is wherever `startxref` pointed. Scan backward
        // from the end to find the "startxref\n<number>" line.
        let original_xref_pos = find_startxref(file.data())
            .ok_or_else(|| invalid_data("could not find startxref in original PDF"))?;

        let xref_kind = detect_xref_kind(file.data(), original_xref_pos);

        Ok(Self {
            doc,
            next_obj_num: size,
            pending: BTreeMap::new(),
            original_xref_pos,
            catalog_ref: root,
            xref_kind,
            info_ref_override: None,
        })
    }

    /// The parsed original document (read-only view; pending edits are *not*
    /// reflected — use [`Self::resolve_current`] for those).
    pub fn document(&self) -> &PdfDocument {
        &self.doc
    }

    /// Add a new indirect object. Returns its `(obj_num, gen_num)`. Generation
    /// is always 0 for newly created objects.
    pub fn add_object(&mut self, obj: &PdfObject) -> (u32, u32) {
        self.try_add_object(obj)
            .expect("object number space is exhausted")
    }

    /// Fallible form of [`Self::add_object`] for callers processing untrusted
    /// or extremely large update batches.
    pub fn try_add_object(&mut self, obj: &PdfObject) -> Result<(u32, u32)> {
        self.ensure_object_capacity(1)?;
        let pending = match obj {
            PdfObject::Stream(stream) => {
                PendingObject::Stream(stream.dict.clone(), Arc::clone(&stream.data))
            }
            _ => PendingObject::Object(obj.clone()),
        };
        let num = self.next_obj_num;
        self.next_obj_num += 1;
        self.pending.insert(num, (0, pending));
        Ok((num, 0))
    }

    /// Add a stream object (a dictionary plus binary data). Returns its object reference.
    pub fn add_stream(&mut self, dict: &PdfDict, data: &[u8]) -> (u32, u32) {
        self.try_add_stream(dict, data)
            .expect("object number space is exhausted")
    }

    /// Fallible form of [`Self::add_stream`].
    pub fn try_add_stream(&mut self, dict: &PdfDict, data: &[u8]) -> Result<(u32, u32)> {
        // Check before copying a potentially large stream into the pending store.
        self.ensure_object_capacity(1)?;
        self.try_add_stream_arc(dict.clone(), Arc::from(data))
    }

    fn try_add_stream_arc(&mut self, dict: PdfDict, data: Arc<[u8]>) -> Result<(u32, u32)> {
        self.ensure_object_capacity(1)?;
        let num = self.next_obj_num;
        self.next_obj_num += 1;
        self.pending
            .insert(num, (0, PendingObject::Stream(dict, data)));
        Ok((num, 0))
    }

    /// Add a stream object with the raw data FlateDecode-compressed (the dict
    /// gains `/Filter /FlateDecode`). Returns its object reference.
    pub fn add_flate_stream(&mut self, dict: &PdfDict, raw: &[u8]) -> (u32, u32) {
        self.try_add_flate_stream(dict, raw)
            .expect("object number space is exhausted")
    }

    /// Fallible form of [`Self::add_flate_stream`]. Capacity is checked before
    /// compression so exhausted writers do not allocate a temporary buffer.
    pub fn try_add_flate_stream(&mut self, dict: &PdfDict, raw: &[u8]) -> Result<(u32, u32)> {
        self.ensure_object_capacity(1)?;
        let mut dict = dict.clone();
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("FlateDecode")),
        );
        self.try_add_stream_arc(dict, flate_compress(raw).into())
    }

    /// Verify that `count` new indirect objects can be assigned without
    /// changing writer state. High-level multi-object edits use this as a
    /// transaction preflight so exhaustion cannot leave half an edit pending.
    pub(crate) fn ensure_object_capacity(&self, count: usize) -> Result<()> {
        let count =
            u32::try_from(count).map_err(|_| invalid_data("object number space is exhausted"))?;
        self.next_obj_num
            .checked_add(count)
            .ok_or_else(|| invalid_data("object number space is exhausted"))?;
        Ok(())
    }

    /// Queue a replacement for an existing object. The replacement is written
    /// with the same object/generation number, shadowing the original.
    pub fn overwrite_object(&mut self, id: ObjectId, obj: PdfObject) {
        self.pending
            .insert(id.0, (id.1, PendingObject::Object(obj)));
    }

    /// Resolve an object as the *update* will see it: the pending (edited)
    /// version when one exists, otherwise the original. All load-modify-store
    /// edits go through this so that successive edits compose.
    pub fn resolve_current(&self, id: ObjectId) -> Result<PdfObject> {
        match self.pending.get(&id.0) {
            Some((_, PendingObject::Object(obj))) => Ok(obj.clone()),
            Some((_, PendingObject::Stream(dict, data))) => {
                Ok(PdfObject::Stream(zpdf_core::PdfStream {
                    dict: dict.clone(),
                    data: Arc::clone(data),
                }))
            }
            None => self.doc.file().resolve(id),
        }
    }

    /// Resolve one level of indirection against the current (pending-aware)
    /// state, returning `Null` on failure.
    pub(crate) fn deref_current(&self, obj: &PdfObject) -> PdfObject {
        match obj {
            PdfObject::Ref(r) => self.resolve_current(*r).unwrap_or(PdfObject::Null),
            other => other.clone(),
        }
    }

    /// Set the trailer `/Info` reference (used when a fresh info dict object
    /// was created instead of overwriting an existing one in place).
    pub(crate) fn set_info_ref(&mut self, id: ObjectId) {
        self.info_ref_override = Some(id);
    }

    /// Append an ink annotation to the specified page. This:
    /// 1. Adds the appearance XObject as a new stream
    /// 2. Adds the annotation dict (with `/AP /N` referencing the XObject) as a new object
    /// 3. Rewrites the page dict with the annotation appended to `/Annots`
    ///
    /// The page is identified by its 0-based index.
    pub fn add_ink_annotation_to_page(
        &mut self,
        page_index: usize,
        annot_dict: &InkAnnotDict,
        appearance_stream: &[u8],
    ) -> Result<()> {
        let page_id = self.page_id(page_index)?;
        self.ensure_object_capacity(2)?;
        let annot_number = self.next_obj_num + 1;
        let updated_page = self.page_with_appended_annot(page_id, ObjectId(annot_number, 0))?;

        // 1. Add the appearance stream (a Form XObject).
        let appearance_dict = self.build_appearance_dict(annot_dict);
        let ap_ref = self.try_add_stream(&appearance_dict, appearance_stream)?;

        // 2. Add the annotation dict.
        let annot_pdf_dict = self.build_annot_dict(annot_dict, ap_ref);
        let annot_ref = self.try_add_object(&PdfObject::Dict(annot_pdf_dict))?;
        debug_assert_eq!(annot_ref, (annot_number, 0));

        // 3. Rewrite the page dict with the annotation appended to /Annots.
        self.overwrite_object(page_id, PdfObject::Dict(updated_page));
        Ok(())
    }

    /// The object id of a 0-based page index (pending edits do not change page
    /// identity, so the original page tree is authoritative).
    pub(crate) fn page_id(&self, page_index: usize) -> Result<ObjectId> {
        Ok(self
            .doc
            .page(page_index)
            .map_err(|_| invalid_data(&format!("page {} not found", page_index)))?
            .id)
    }

    /// Build a page dict with one more `/Annots` entry.
    fn page_with_appended_annot(&self, page_id: ObjectId, annot_ref: ObjectId) -> Result<PdfDict> {
        let page_obj = self.resolve_current(page_id)?;
        let mut page_dict = page_obj.as_dict()?.clone();

        // Get the existing /Annots array (or create a new one), following one
        // level of indirection.
        let mut annots = match page_dict.get("Annots") {
            Some(PdfObject::Ref(r)) => match self.resolve_current(*r) {
                Ok(obj) => obj.as_array().ok().map(|a| a.to_vec()).unwrap_or_default(),
                Err(_) => Vec::new(),
            },
            Some(PdfObject::Array(arr)) => arr.to_vec(),
            _ => Vec::new(),
        };
        annots.push(PdfObject::Ref(annot_ref));
        page_dict.insert(PdfName::new("Annots"), PdfObject::Array(annots));
        Ok(page_dict)
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
                    .flat_map(|&(x, y)| [PdfObject::Real(x), PdfObject::Real(y)])
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

    /// Write the incremental update to the output. The output must be seekable
    /// (to record xref offsets).
    pub fn write<W: Write + Seek>(&self, mut out: W) -> io::Result<()> {
        // 1. Copy the original PDF, ensuring the first new object starts on a
        //    fresh line.
        let original = self.doc.file().data();
        out.write_all(original)?;
        if !matches!(original.last(), Some(b'\n') | Some(b'\r')) {
            out.write_all(b"\n")?;
        }

        // 2. Append pending objects in object-number order, recording offsets.
        let mut xref_entries: Vec<(u32, u16, u64)> = Vec::new();
        for (&num, (gen, pending)) in &self.pending {
            let offset = out.stream_position()?;
            xref_entries.push((num, *gen, offset));
            match pending {
                PendingObject::Object(obj) => write_object(&mut out, num, *gen as u32, obj)?,
                PendingObject::Stream(dict, data) => {
                    write_stream(&mut out, num, *gen as u32, dict, data)?
                }
            }
        }

        // 3. Append the new cross-reference section + trailer, matching the
        //    original file's flavor.
        let xref_pos = out.stream_position()?;
        match self.xref_kind {
            XrefKind::Table => {
                self.write_xref_table(&mut out, &xref_entries)?;
                self.write_trailer(&mut out)?;
            }
            XrefKind::Stream => self.write_xref_stream(&mut out, &xref_entries, xref_pos)?,
        }

        writeln!(out, "startxref")?;
        writeln!(out, "{}", xref_pos)?;
        writeln!(out, "%%EOF")?;
        Ok(())
    }

    /// The trailer entries shared by both xref flavors: `/Size`, `/Prev`,
    /// `/Root`, plus `/Info` and `/ID` carried over from the original trailer
    /// (or the freshly created `/Info`).
    fn trailer_dict(&self, size: u32) -> PdfDict {
        let mut trailer = PdfDict::new();
        trailer.insert(PdfName::new("Size"), PdfObject::Integer(size as i64));
        trailer.insert(
            PdfName::new("Prev"),
            PdfObject::Integer(self.original_xref_pos as i64),
        );
        trailer.insert(PdfName::new("Root"), PdfObject::Ref(self.catalog_ref));
        let orig = &self.doc.file().trailer;
        match self.info_ref_override {
            Some(id) => {
                trailer.insert(PdfName::new("Info"), PdfObject::Ref(id));
            }
            None => {
                if let Some(PdfObject::Ref(r)) = orig.get("Info") {
                    trailer.insert(PdfName::new("Info"), PdfObject::Ref(*r));
                }
            }
        }
        if let Some(id) = orig.get("ID") {
            trailer.insert(PdfName::new("ID"), id.clone());
        }
        trailer
    }

    /// Write the classic xref table for the new objects.
    fn write_xref_table<W: Write>(
        &self,
        out: &mut W,
        entries: &[(u32, u16, u64)],
    ) -> io::Result<()> {
        writeln!(out, "xref")?;
        for run in contiguous_runs(entries) {
            writeln!(out, "{} {}", run[0].0, run.len())?;
            for (_, gen, offset) in run {
                if *offset > 9_999_999_999 {
                    return Err(invalid_data(
                        "classic xref offsets cannot exceed ten decimal digits",
                    ));
                }
                writeln!(out, "{:010} {:05} n ", offset, gen)?;
            }
        }
        Ok(())
    }

    /// Write the new trailer (classic-table flavor).
    fn write_trailer<W: Write>(&self, out: &mut W) -> io::Result<()> {
        writeln!(out, "trailer")?;
        serialize_dict(out, &self.trailer_dict(self.next_obj_num))?;
        writeln!(out)?;
        Ok(())
    }

    /// Write an update cross-reference *stream* (ISO 32000-1 §7.5.8): a stream
    /// object that is its own xref section and trailer. The stream object gets
    /// the next free object number and contains an entry for itself.
    fn write_xref_stream<W: Write>(
        &self,
        out: &mut W,
        entries: &[(u32, u16, u64)],
        xref_pos: u64,
    ) -> io::Result<()> {
        let stream_num = self.next_obj_num;

        // All entries incl. the xref stream itself, sorted by object number
        // (pending entries are pre-sorted; the self entry is the largest).
        let mut all: Vec<(u32, u16, u64)> = entries.to_vec();
        all.push((stream_num, 0, xref_pos));

        // Use an 8-byte offset field. Four bytes silently corrupt xref streams
        // once an incremental update crosses 4 GiB, while /W permits 64-bit
        // fields and the parser already supports them.
        let data_capacity = all
            .len()
            .checked_mul(11)
            .ok_or_else(|| invalid_data("xref stream is too large"))?;
        let mut data = Vec::with_capacity(data_capacity);
        let mut index = Vec::with_capacity(all.len().saturating_mul(2));
        for run in contiguous_runs(&all) {
            index.push(PdfObject::Integer(run[0].0 as i64));
            index.push(PdfObject::Integer(run.len() as i64));
            for (_, gen, offset) in run {
                data.push(1u8); // type 1: in-use, uncompressed
                data.extend_from_slice(&offset.to_be_bytes());
                data.extend_from_slice(&gen.to_be_bytes());
            }
        }

        let size = stream_num
            .checked_add(1)
            .ok_or_else(|| invalid_data("object number space is exhausted"))?;
        let mut dict = self.trailer_dict(size);
        dict.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("XRef")));
        dict.insert(
            PdfName::new("W"),
            PdfObject::Array(vec![
                PdfObject::Integer(1),
                PdfObject::Integer(8),
                PdfObject::Integer(2),
            ]),
        );
        dict.insert(PdfName::new("Index"), PdfObject::Array(index));
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("FlateDecode")),
        );

        write_stream(out, stream_num, 0, &dict, &flate_compress(&data))
    }

    /// Delete an annotation from a page's `/Annots` array. The annotation object
    /// itself remains in the file (orphaned), but will no longer be rendered.
    ///
    /// # Arguments
    /// * `page_index` - 0-based page index
    /// * `annot_id` - Object ID of the annotation to remove
    ///
    /// # Example
    /// ```no_run
    /// # use zpdf_writer::IncrementalWriter;
    /// # use zpdf_core::ObjectId;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let pdf_bytes = std::fs::read("input.pdf")?;
    /// let mut writer = IncrementalWriter::new(pdf_bytes)?;
    /// writer.delete_annotation(0, ObjectId(5, 0))?;
    /// writer.write(&mut std::fs::File::create("output.pdf")?)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn delete_annotation(&mut self, page_index: usize, annot_id: ObjectId) -> Result<()> {
        let page_id = self.page_id(page_index)?;
        let page_obj = self.resolve_current(page_id)?;
        let mut page_dict = page_obj.as_dict()?.clone();

        // Get the existing /Annots array
        let annots = match page_dict.get("Annots") {
            Some(PdfObject::Ref(r)) => match self.resolve_current(*r) {
                Ok(obj) => obj.as_array().ok().map(|a| a.to_vec()).unwrap_or_default(),
                Err(_) => Vec::new(),
            },
            Some(PdfObject::Array(arr)) => arr.to_vec(),
            _ => return Ok(()), // No annotations to delete
        };

        // Filter out the annotation to delete
        let filtered: Vec<PdfObject> = annots
            .into_iter()
            .filter(|obj| match obj {
                PdfObject::Ref(r) => *r != annot_id,
                _ => true,
            })
            .collect();

        page_dict.insert(PdfName::new("Annots"), PdfObject::Array(filtered));
        self.overwrite_object(page_id, PdfObject::Dict(page_dict));
        Ok(())
    }

    /// Update an annotation's bounding rectangle. This effectively moves or
    /// resizes the annotation.
    ///
    /// # Arguments
    /// * `annot_id` - Object ID of the annotation to update
    /// * `new_rect` - New rectangle in PDF coordinates (bottom-left origin)
    ///
    /// # Note
    /// For annotations with appearance streams, this only updates the `/Rect`.
    /// The appearance stream itself is not regenerated. For ink annotations and
    /// stamps where the appearance uses relative coordinates within the BBox,
    /// this is sufficient.
    ///
    /// # Example
    /// ```no_run
    /// # use zpdf_writer::IncrementalWriter;
    /// # use zpdf_core::{ObjectId, Rect};
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let pdf_bytes = std::fs::read("input.pdf")?;
    /// let mut writer = IncrementalWriter::new(pdf_bytes)?;
    /// let new_rect = Rect { x0: 100.0, y0: 200.0, x1: 300.0, y1: 400.0 };
    /// writer.update_annotation_rect(ObjectId(5, 0), new_rect)?;
    /// writer.write(&mut std::fs::File::create("output.pdf")?)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_annotation_rect(
        &mut self,
        annot_id: ObjectId,
        new_rect: zpdf_core::Rect,
    ) -> Result<()> {
        let annot_obj = self.resolve_current(annot_id)?;
        let mut annot_dict = annot_obj.as_dict()?.clone();

        annot_dict.insert(
            PdfName::new("Rect"),
            PdfObject::Array(vec![
                PdfObject::Real(new_rect.x0),
                PdfObject::Real(new_rect.y0),
                PdfObject::Real(new_rect.x1),
                PdfObject::Real(new_rect.y1),
            ]),
        );

        self.overwrite_object(annot_id, PdfObject::Dict(annot_dict));
        Ok(())
    }

    /// Update an annotation's color (the `/C` entry, typically stroke color).
    ///
    /// # Arguments
    /// * `annot_id` - Object ID of the annotation to update
    /// * `color` - RGB color as (r, g, b) tuple with values in [0.0, 1.0]
    ///
    /// # Note
    /// This updates the annotation dictionary but does not regenerate the
    /// appearance stream. For full visual effect, the annotation's appearance
    /// would need to be regenerated.
    ///
    /// # Example
    /// ```no_run
    /// # use zpdf_writer::IncrementalWriter;
    /// # use zpdf_core::ObjectId;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let pdf_bytes = std::fs::read("input.pdf")?;
    /// let mut writer = IncrementalWriter::new(pdf_bytes)?;
    /// writer.update_annotation_color(ObjectId(5, 0), (1.0, 0.0, 0.0))?; // Red
    /// writer.write(&mut std::fs::File::create("output.pdf")?)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_annotation_color(
        &mut self,
        annot_id: ObjectId,
        color: (f64, f64, f64),
    ) -> Result<()> {
        let annot_obj = self.resolve_current(annot_id)?;
        let mut annot_dict = annot_obj.as_dict()?.clone();

        annot_dict.insert(
            PdfName::new("C"),
            PdfObject::Array(vec![
                PdfObject::Real(color.0),
                PdfObject::Real(color.1),
                PdfObject::Real(color.2),
            ]),
        );

        self.overwrite_object(annot_id, PdfObject::Dict(annot_dict));
        Ok(())
    }

    /// Update a text annotation's content (the `/Contents` entry).
    ///
    /// # Arguments
    /// * `annot_id` - Object ID of the annotation to update
    /// * `content` - New text content
    ///
    /// # Example
    /// ```no_run
    /// # use zpdf_writer::IncrementalWriter;
    /// # use zpdf_core::ObjectId;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let pdf_bytes = std::fs::read("input.pdf")?;
    /// let mut writer = IncrementalWriter::new(pdf_bytes)?;
    /// writer.update_annotation_contents(ObjectId(5, 0), "Updated comment")?;
    /// writer.write(&mut std::fs::File::create("output.pdf")?)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_annotation_contents(&mut self, annot_id: ObjectId, content: &str) -> Result<()> {
        let annot_obj = self.resolve_current(annot_id)?;
        let mut annot_dict = annot_obj.as_dict()?.clone();

        // Use UTF-16BE encoding for non-ASCII text (PDF text strings)
        let encoded = if content.is_ascii() {
            content.as_bytes().to_vec()
        } else {
            let mut bytes = vec![0xfe, 0xff]; // UTF-16BE BOM
            for ch in content.encode_utf16() {
                bytes.push((ch >> 8) as u8);
                bytes.push((ch & 0xff) as u8);
            }
            bytes
        };

        annot_dict.insert(
            PdfName::new("Contents"),
            PdfObject::String(zpdf_core::PdfString(encoded)),
        );

        self.overwrite_object(annot_id, PdfObject::Dict(annot_dict));
        Ok(())
    }

    /// Update an annotation's border width (the `/BS /W` entry).
    ///
    /// # Arguments
    /// * `annot_id` - Object ID of the annotation to update
    /// * `width` - New border width in points (minimum 0.1)
    ///
    /// # Example
    /// ```no_run
    /// # use zpdf_writer::IncrementalWriter;
    /// # use zpdf_core::ObjectId;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let pdf_bytes = std::fs::read("input.pdf")?;
    /// let mut writer = IncrementalWriter::new(pdf_bytes)?;
    /// writer.update_annotation_border_width(ObjectId(5, 0), 3.0)?;
    /// writer.write(&mut std::fs::File::create("output.pdf")?)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_annotation_border_width(&mut self, annot_id: ObjectId, width: f64) -> Result<()> {
        let annot_obj = self.resolve_current(annot_id)?;
        let mut annot_dict = annot_obj.as_dict()?.clone();

        let width = width.max(0.1); // Minimum width

        // Get or create the /BS dictionary
        let mut bs = match annot_dict.get("BS") {
            Some(PdfObject::Dict(d)) => d.clone(),
            _ => PdfDict::new(),
        };

        bs.insert(PdfName::new("W"), PdfObject::Real(width));
        annot_dict.insert(PdfName::new("BS"), PdfObject::Dict(bs));

        self.overwrite_object(annot_id, PdfObject::Dict(annot_dict));
        Ok(())
    }
}

/// Group xref entries into runs of contiguous object numbers (subsections).
fn contiguous_runs(entries: &[(u32, u16, u64)]) -> Vec<&[(u32, u16, u64)]> {
    let mut runs = Vec::new();
    let mut start = 0;
    for i in 1..=entries.len() {
        if i == entries.len() || entries[i - 1].0.checked_add(1) != Some(entries[i].0) {
            runs.push(&entries[start..i]);
            start = i;
        }
    }
    runs
}

/// FlateDecode (zlib) compression for new streams.
fn flate_compress(raw: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    // Vec-backed writes have no I/O failure mode; surfacing an impossible
    // encoder failure is preferable to silently emitting an empty stream.
    enc.write_all(raw)
        .expect("Vec-backed zlib compression cannot fail");
    enc.finish()
        .expect("Vec-backed zlib finalization cannot fail")
}

pub(crate) fn invalid_data(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub(crate) fn unsupported(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, msg.to_string())
}

/// Find the `startxref` offset in a PDF file by scanning backward from the end.
fn find_startxref(data: &[u8]) -> Option<u64> {
    const MARKER: &[u8] = b"startxref";
    // Search the whole file, matching the parser. Although conforming writers
    // put `startxref` near EOF, real files can carry long trailing appends or
    // junk and are still accepted by `PdfFile`; the incremental writer must not
    // reject that same input. Work on raw bytes so malformed UTF-8 is harmless.
    let marker_pos = data
        .windows(MARKER.len())
        .rposition(|window| window == MARKER)?;
    let after = &data[marker_pos + MARKER.len()..];
    let number_start = after.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let digits = &after[number_start..];
    let number_len = digits
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if number_len == 0 {
        return None;
    }
    std::str::from_utf8(&digits[..number_len])
        .ok()?
        .parse::<u64>()
        .ok()
}

/// Classic `xref` table vs. cross-reference stream, sniffed at the original
/// startxref target. Anything that does not begin with the `xref` keyword is
/// an indirect object holding an xref stream.
fn detect_xref_kind(data: &[u8], pos: u64) -> XrefKind {
    let start = usize::try_from(pos).unwrap_or(data.len()).min(data.len());
    let slice = &data[start..];
    let trimmed = slice
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|i| &slice[i..])
        .unwrap_or(&[]);
    if trimmed.starts_with(b"xref") {
        XrefKind::Table
    } else {
        XrefKind::Stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zpdf_core::{ObjectId, Rect};

    /// Create a minimal test PDF with one annotation for testing
    fn create_test_pdf_with_annotation() -> Vec<u8> {
        // Minimal PDF with one page and one ink annotation
        let pdf = b"%PDF-1.4
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Annots [4 0 R] >>
endobj
4 0 obj
<< /Type /Annot /Subtype /Ink /Rect [100 100 200 200] /C [0 0 0] /InkList [[[100 100 200 200]]] /BS << /W 2 >> >>
endobj
xref
0 5
0000000000 65535 f
0000000009 00000 n
0000000058 00000 n
0000000115 00000 n
0000000223 00000 n
trailer
<< /Size 5 /Root 1 0 R >>
startxref
362
%%EOF
";
        pdf.to_vec()
    }

    #[test]
    fn test_delete_annotation() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();

        // Delete annotation 4 0 R from page 0
        writer.delete_annotation(0, ObjectId(4, 0)).unwrap();

        // Write to output
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();

        // Parse the output and verify annotation is gone
        let doc = PdfDocument::open(output.into_inner()).unwrap();
        let page = doc.page(0).unwrap();

        // Page should have no annotations (or empty array)
        let annots = page.annots.len();
        assert_eq!(annots, 0, "Annotation should be deleted");
    }

    #[test]
    fn test_update_annotation_rect() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();

        let new_rect = Rect {
            x0: 150.0,
            y0: 250.0,
            x1: 350.0,
            y1: 450.0,
        };

        // Update annotation 4 0 R rect - should not error
        writer
            .update_annotation_rect(ObjectId(4, 0), new_rect)
            .unwrap();

        // Write to output - should succeed
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();

        // Verify the output is valid PDF
        let result = PdfDocument::open(output.into_inner());
        assert!(result.is_ok(), "Updated PDF should be valid");
    }

    #[test]
    fn test_update_annotation_color() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();

        // Update to red color - should not error
        writer
            .update_annotation_color(ObjectId(4, 0), (1.0, 0.0, 0.0))
            .unwrap();

        // Write to output - should succeed
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();

        // Verify the output is valid PDF
        let result = PdfDocument::open(output.into_inner());
        assert!(result.is_ok(), "Updated PDF should be valid");
    }

    #[test]
    fn test_update_annotation_border_width() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();

        // Update border width - should not error
        writer
            .update_annotation_border_width(ObjectId(4, 0), 5.0)
            .unwrap();

        // Write to output - should succeed
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();

        // Verify the output is valid PDF
        let result = PdfDocument::open(output.into_inner());
        assert!(result.is_ok(), "Updated PDF should be valid");
    }

    #[test]
    fn stale_trailer_size_does_not_reuse_an_existing_object_number() {
        let pdf = create_test_pdf_with_annotation();
        let pdf = String::from_utf8(pdf)
            .unwrap()
            .replace("<< /Size 5 /Root", "<< /Size 2 /Root")
            .into_bytes();
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        let (number, generation) = writer.add_object(&PdfObject::Integer(42));
        assert_eq!((number, generation), (5, 0));
    }

    #[test]
    fn generic_add_object_accepts_streams_without_panicking() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        let stream = zpdf_core::PdfStream::new(PdfDict::new(), b"hello".to_vec());
        let (number, generation) = writer.add_object(&PdfObject::Stream(stream));

        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();
        let doc = PdfDocument::open(output.into_inner()).unwrap();
        let object = doc
            .file()
            .resolve(ObjectId(number, generation as u16))
            .unwrap();
        assert_eq!(object.as_stream().unwrap().data.as_ref(), b"hello");
    }

    #[test]
    fn fallible_add_rejects_object_number_exhaustion_without_mutating() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        writer.next_obj_num = u32::MAX;
        assert!(writer.try_add_object(&PdfObject::Integer(42)).is_err());
        assert!(writer.pending.is_empty());
        assert_eq!(writer.next_obj_num, u32::MAX);
    }

    #[test]
    fn fallible_stream_adds_reject_exhaustion_without_mutating() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        writer.next_obj_num = u32::MAX;

        assert!(writer
            .try_add_stream(&PdfDict::new(), b"uncompressed")
            .is_err());
        assert!(writer
            .try_add_flate_stream(&PdfDict::new(), b"compressed")
            .is_err());
        assert!(writer.pending.is_empty());
        assert_eq!(writer.next_obj_num, u32::MAX);
    }

    #[test]
    fn annotation_preflights_all_object_numbers() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        writer.next_obj_num = u32::MAX - 1;
        let annot = InkAnnotDict {
            rect: Rect::new(10.0, 10.0, 20.0, 20.0),
            ink_list: vec![vec![(10.0, 10.0), (20.0, 20.0)]],
            color: (0.0, 0.0, 0.0),
            width: 1.0,
        };

        assert!(writer
            .add_ink_annotation_to_page(0, &annot, b"q Q")
            .is_err());
        assert!(writer.pending.is_empty());
        assert_eq!(writer.next_obj_num, u32::MAX - 1);
    }

    #[test]
    fn new_info_object_reports_exhaustion_without_mutating() {
        let pdf = create_test_pdf_with_annotation();
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        writer.next_obj_num = u32::MAX;

        assert!(writer.set_info(&InfoUpdate::default()).is_err());
        assert!(writer.pending.is_empty());
        assert!(writer.info_ref_override.is_none());
        assert_eq!(writer.next_obj_num, u32::MAX);
    }

    #[test]
    fn startxref_scan_accepts_whitespace_and_non_utf8_tail_bytes() {
        let mut data = vec![0xff; 32];
        data.extend_from_slice(b"startxref \r\n\t 12345\n%%EOF");
        assert_eq!(find_startxref(&data), Some(12_345));
    }

    #[test]
    fn startxref_scan_accepts_long_trailing_data() {
        let mut data = b"startxref\n12345\n%%EOF\n".to_vec();
        data.extend(std::iter::repeat_n(0xff, 2_048));
        assert_eq!(find_startxref(&data), Some(12_345));
    }
}
