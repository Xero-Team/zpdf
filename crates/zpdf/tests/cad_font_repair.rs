//! CAD font recovery (PR #15) end to end on the CPU backend: a broken
//! Identity-H subset (zero-length `loca` entries) draws exactly what its
//! validated sibling draws, and a composite show leaked under a simple font
//! draws the glyphs it was typeset for.
#![cfg(feature = "cpu-render")]

use std::sync::Arc;

use zpdf::{ContentInterpreter, FontProvider, ImageCache, PdfDocument, RenderBackend};

const CAD: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/cad-subset.ttf");
const CAD_BROKEN: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/cad-subset-broken.ttf");
const PRIMARY: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

const TO_UNICODE: &[u8] = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
    1 begincodespacerange <0000> <FFFF> endcodespacerange \
    5 beginbfchar <0001> <0020> <0002> <0041> <0003> <0042> <0004> <0043> <0005> <0044> endbfchar \
    endcmap CMapName currentdict /CMap defineresource pop end end";

fn assemble(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let count = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

fn stream(dict: &str, bytes: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
    object.extend_from_slice(bytes);
    object.extend_from_slice(b"\nendstream");
    object
}

enum Font {
    Type0(&'static [u8], &'static str),
    Simple(&'static [u8], &'static str),
}

/// Font objects appended to `objects`; returns the font dictionary's id.
fn push_font(objects: &mut Vec<Vec<u8>>, font: &Font) -> usize {
    let first = objects.len() + 1;
    match *font {
        Font::Type0(program, name) => {
            objects.push(
                format!(
                    "<< /Type /Font /Subtype /Type0 /BaseFont /{name} /Encoding /Identity-H \
                     /DescendantFonts [{} 0 R] /ToUnicode {} 0 R >>",
                    first + 1,
                    first + 4
                )
                .into_bytes(),
            );
            objects.push(
                format!(
                    "<< /Type /Font /Subtype /CIDFontType2 /BaseFont /{name} \
                     /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
                     /FontDescriptor {} 0 R /DW 1000 /W [1 [350]] /CIDToGIDMap /Identity >>",
                    first + 2
                )
                .into_bytes(),
            );
            objects.push(
                format!(
                    "<< /Type /FontDescriptor /FontName /{name} /Flags 4 \
                     /FontBBox [0 -200 1000 900] /ItalicAngle 0 /Ascent 900 /Descent -200 \
                     /CapHeight 700 /StemV 80 /FontFile2 {} 0 R >>",
                    first + 3
                )
                .into_bytes(),
            );
            objects.push(stream(&format!("/Length1 {}", program.len()), program));
            objects.push(stream("", TO_UNICODE));
        }
        Font::Simple(program, name) => {
            objects.push(
                format!(
                    "<< /Type /Font /Subtype /TrueType /BaseFont /{name} \
                     /FontDescriptor {} 0 R >>",
                    first + 1
                )
                .into_bytes(),
            );
            objects.push(
                format!(
                    "<< /Type /FontDescriptor /FontName /{name} /Flags 32 \
                     /FontBBox [0 -200 1000 900] /ItalicAngle 0 /Ascent 900 /Descent -200 \
                     /CapHeight 700 /StemV 80 /FontFile2 {} 0 R >>",
                    first + 2
                )
                .into_bytes(),
            );
            objects.push(stream(&format!("/Length1 {}", program.len()), program));
        }
    }
    first
}

/// One page whose resources name each font in order (`/F0`, `/F1`, …).
fn pdf(content: &[u8], fonts: &[Font]) -> Vec<u8> {
    pdf_with_form(content, fonts, None)
}

/// [`pdf`] plus a Form XObject `/Fm` whose own resources name its fonts
/// `/G0`, `/G1`, ….
fn pdf_with_form(content: &[u8], fonts: &[Font], form: Option<(&[u8], &[Font])>) -> Vec<u8> {
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        Vec::new(),
        stream("", content),
    ];
    let mut names = String::new();
    for (index, font) in fonts.iter().enumerate() {
        let id = push_font(&mut objects, font);
        names.push_str(&format!("/F{index} {id} 0 R "));
    }
    let mut xobjects = String::new();
    if let Some((form_content, form_fonts)) = form {
        let mut form_names = String::new();
        for (index, font) in form_fonts.iter().enumerate() {
            let id = push_font(&mut objects, font);
            form_names.push_str(&format!("/G{index} {id} 0 R "));
        }
        objects.push(stream(
            &format!(
                "/Type /XObject /Subtype /Form /BBox [0 0 240 160] \
                 /Resources << /Font << {form_names}>> >>"
            ),
            form_content,
        ));
        xobjects = format!("/XObject << /Fm {} 0 R >>", objects.len());
    }
    objects[2] = format!(
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 160] /Contents 4 0 R \
         /Resources << /Font << {names}>> {xobjects} >> >>"
    )
    .into_bytes();
    assemble(&objects)
}

/// RGBA pixels of page 0 at 1 px/pt.
fn render(pdf: Vec<u8>, provider: Option<Arc<FontProvider>>) -> Vec<u8> {
    let mut document = PdfDocument::open(pdf).expect("open pdf");
    if let Some(provider) = provider {
        document.set_font_provider(provider);
    }
    let page = document.page(0).expect("page 0");
    let mut fonts = document.load_page_fonts(&page);
    let content = document.page_content_bytes(&page).expect("content");
    let mut images = ImageCache::new();
    let list = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(document.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);
    zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&list, 1.0)
        .expect("cpu render")
        .data
}

fn dark(pixels: &[u8]) -> usize {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] < 128 && pixel[3] > 0)
        .count()
}

fn provider() -> Arc<FontProvider> {
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(PRIMARY, 0).unwrap();
    provider
}

#[test]
fn broken_subset_draws_its_validated_siblings_glyphs() {
    let fonts = [
        Font::Type0(CAD_BROKEN, "ABCDEF+Iso"),
        Font::Type0(CAD, "GHIJKL+Gost"),
    ];
    let text = b"<0002000100030004>";
    let shown = |font: &str| {
        [
            b"BT /",
            font.as_bytes(),
            b" 48 Tf 10 60 Td ",
            text,
            b" Tj ET",
        ]
        .concat()
    };
    let reference = render(pdf(&shown("F1"), &fonts), None);
    assert!(dark(&reference) > 300, "{}", dark(&reference));
    // With and without a provider, the sibling supplies the exact glyphs.
    assert_eq!(render(pdf(&shown("F0"), &fonts), None), reference);
    assert_eq!(
        render(pdf(&shown("F0"), &fonts), Some(provider())),
        reference
    );

    // Alone, the broken subset still draws nothing, unless registered
    // coverage can stand in for its letters.
    let alone = [Font::Type0(CAD_BROKEN, "ABCDEF+Iso")];
    assert_eq!(dark(&render(pdf(&shown("F0"), &alone), None)), 0);
    let recovered = dark(&render(pdf(&shown("F0"), &alone), Some(provider())));
    assert!(recovered > 300, "{recovered}");
}

#[test]
fn leaked_composite_show_draws_the_intended_glyphs() {
    let fonts = [Font::Type0(CAD, "GHIJKL+Gost"), Font::Simple(CAD, "Simple")];
    let page = |leaked: &str| {
        [
            b"BT /F0 48 Tf 10 100 Td <0002> Tj ET BT /".as_slice(),
            leaked.as_bytes(),
            b" 48 Tf 10 20 Td <00020003> Tj ET",
        ]
        .concat()
    };
    let reference = render(pdf(&page("F0"), &fonts), None);
    let leaked = render(pdf(&page("F1"), &fonts), None);
    assert!(dark(&reference) > 300, "{}", dark(&reference));
    assert_eq!(leaked, reference, "no .notdef boxes for the leaked show");
}

#[test]
fn form_fonts_are_siblings_only_inside_the_form() {
    let page_fonts = [Font::Type0(CAD_BROKEN, "ABCDEF+Iso")];
    let form_fonts = [Font::Type0(CAD, "GHIJKL+Gost")];
    let shown =
        |font: &str, y: u32| format!("BT /{font} 48 Tf 10 {y} Td <00020003> Tj ET").into_bytes();
    let page = |content: &[u8], form: &[u8]| {
        render(
            pdf_with_form(content, &page_fonts, Some((form, &form_fonts))),
            None,
        )
    };
    // Inside the Form, its own font is in scope and supplies the glyphs.
    let reference = page(b"/Fm Do", &shown("G0", 60));
    assert!(dark(&reference) > 300, "{}", dark(&reference));
    assert_eq!(page(b"/Fm Do", &shown("F0", 60)), reference);
    // After the Form ends, its font is out of scope again: a decision made
    // inside the Form is not reused for the page's own broken text.
    let after = [b"/Fm Do ".as_slice(), &shown("F0", 110)].concat();
    assert_eq!(page(&after, &shown("F0", 60)), reference);
}

#[test]
fn clip_mode_text_clips_with_repaired_glyphs() {
    let fonts = [
        Font::Type0(CAD_BROKEN, "ABCDEF+Iso"),
        Font::Type0(CAD, "GHIJKL+Gost"),
    ];
    let page = |font: &str| {
        format!(
            "q BT 7 Tr /{font} 48 Tf 10 60 Td <000200030004> Tj ET \
             0 0 0 rg 0 0 240 160 re f Q"
        )
        .into_bytes()
    };
    let reference = render(pdf(&page("F1"), &fonts), None);
    let painted = dark(&reference);
    assert!(painted > 300 && painted < 240 * 160 / 2, "{painted}");
    assert_eq!(render(pdf(&page("F0"), &fonts), None), reference);
}
