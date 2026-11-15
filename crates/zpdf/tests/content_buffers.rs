//! Byte/limit behavior and a large-allocation regression for content assembly.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use zpdf::{Error, ObjectId, ParseLimits, PdfDict, PdfDocument, PdfName, PdfObject};
use zpdf_writer::{DocumentBuilder, IncrementalWriter};

const LARGE: usize = 4 * 1024 * 1024;
thread_local! {
    static LARGE_ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}
struct Counting;
fn record(size: usize) {
    if size >= LARGE {
        let _ = LARGE_ALLOCATIONS.try_with(|n| n.set(n.get().wrapping_add(1)));
    }
}
// SAFETY: the allocator forwards pointers and layouts unchanged. Non-dropping,
// const-initialized TLS counts only this test's thread and cannot allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(p, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn document(streams: &[(&[u8], bool)], limit: u64) -> PdfDocument {
    let mut builder = DocumentBuilder::new();
    builder.add_page(20.0, 20.0);
    let mut writer = IncrementalWriter::new(builder.build().unwrap()).unwrap();
    let page = writer.document().page(0).unwrap();
    let mut refs = Vec::new();
    for &(bytes, damaged) in streams {
        let mut dict = PdfDict::new();
        if damaged {
            dict.insert(
                PdfName::new("Filter"),
                PdfObject::Name(PdfName::new("UnsupportedDecode")),
            );
        }
        let (num, gen) = writer.try_add_stream(&dict, bytes).unwrap();
        refs.push(PdfObject::Ref(ObjectId(num, gen as u16)));
    }
    let mut page_dict = writer
        .resolve_current(page.id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    page_dict.insert(PdfName::new("Contents"), PdfObject::Array(refs));
    writer.overwrite_object(page.id, PdfObject::Dict(page_dict));
    let mut out = std::io::Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    PdfDocument::open_with_limits(
        out.into_inner(),
        ParseLimits {
            max_decoded_stream_bytes: limit,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn resolving_a_raw_stream_does_not_copy_its_payload() {
    let data = vec![b' '; LARGE];
    let doc = document(&[(&data, false)], LARGE as u64);
    let id = doc.page(0).unwrap().contents[0];
    let before = LARGE_ALLOCATIONS.with(Cell::get);
    let object = doc.file().resolve(id).unwrap();
    let count = LARGE_ALLOCATIONS.with(Cell::get).wrapping_sub(before);
    assert_eq!(object.as_stream().unwrap().data.as_ref(), data.as_slice());
    assert_eq!(
        count, 0,
        "raw payload must be a view of the file allocation"
    );
}

#[test]
fn single_stream_needs_only_one_decoded_buffer() {
    let data = vec![b' '; LARGE];
    let doc = document(&[(&data, false)], LARGE as u64);
    let page = doc.page(0).unwrap();
    // Populate parsed-object caches before counting the decoded output itself.
    drop(doc.page_content_bytes(&page).unwrap());
    let before = LARGE_ALLOCATIONS.with(Cell::get);
    let content = doc.page_content_bytes(&page).unwrap();
    let count = LARGE_ALLOCATIONS.with(Cell::get).wrapping_sub(before);
    assert_eq!(content, data);
    assert_eq!(
        count, 1,
        "a single stream must not be copied into a second large buffer"
    );
}

#[test]
fn empty_streams_preserve_existing_separators() {
    let doc = document(
        &[
            (b"", false),
            (b"A", false),
            (b"", false),
            (b"B", false),
            (b"", false),
        ],
        6,
    );
    assert_eq!(
        doc.page_content_bytes(&doc.page(0).unwrap()).unwrap(),
        b"A\n\nB\n"
    );
    let doc = document(&[(b"", false), (b"", false)], 0);
    assert!(doc
        .page_content_bytes(&doc.page(0).unwrap())
        .unwrap()
        .is_empty());
}

#[test]
fn surviving_streams_keep_diagnostics_and_aggregate_limit() {
    for streams in [
        [(b"X".as_slice(), true), (b"AA", false), (b"B", false)],
        [(b"AA".as_slice(), false), (b"X", true), (b"B", false)],
    ] {
        let doc = document(&streams, 4);
        let page = doc.page(0).unwrap();
        let (bytes, diagnostics) = doc.page_content_with_diagnostics(&page).unwrap();
        assert_eq!(bytes, b"AA\nB");
        assert_eq!(diagnostics.decode_failures, 1);
        assert!(matches!(
            doc.page_content_bytes(&page),
            Err(Error::IncompleteOutput(_))
        ));
        let doc = document(&streams, 3);
        assert!(matches!(
            doc.page_content_with_diagnostics(&doc.page(0).unwrap()),
            Err(Error::StreamSizeLimit(3))
        ));
    }
}
