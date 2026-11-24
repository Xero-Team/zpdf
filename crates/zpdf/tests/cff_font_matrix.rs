//! End-to-end rendering regression tests for CFF top-level and per-FD `FontMatrix` (#160).
#![cfg(feature = "cpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

fn assemble_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    out
}

fn stream_obj(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

fn encode_cff_int(buf: &mut Vec<u8>, v: i32) {
    buf.push(29);
    buf.extend_from_slice(&v.to_be_bytes());
}

fn encode_cff_real(buf: &mut Vec<u8>, s: &str) {
    buf.push(30);
    let mut nibbles = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => nibbles.push(bytes[i] - b'0'),
            b'.' => nibbles.push(0xa),
            b'-' => nibbles.push(0xe),
            b'e' | b'E' => {
                if bytes.get(i + 1) == Some(&b'-') {
                    nibbles.push(0xc);
                    i += 1;
                } else {
                    nibbles.push(0xb);
                }
            }
            other => panic!("unexpected char: {}", other as char),
        }
        i += 1;
    }
    nibbles.push(0xf);
    if nibbles.len() % 2 == 1 {
        nibbles.push(0xf);
    }
    for pair in nibbles.as_chunks::<2>().0 {
        buf.push((pair[0] << 4) | pair[1]);
    }
}

fn encode_cff_matrix(buf: &mut Vec<u8>, m: &[&str; 6]) {
    for s in m {
        encode_cff_real(buf, s);
    }
    buf.extend_from_slice(&[12, 7]);
}

fn encode_cff_index(items: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(items.len() as u16).to_be_bytes());
    if items.is_empty() {
        return out;
    }
    out.push(2);
    let mut offset = 1u16;
    out.extend_from_slice(&offset.to_be_bytes());
    for item in items {
        offset = offset.checked_add(item.len() as u16).unwrap();
        out.extend_from_slice(&offset.to_be_bytes());
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// 800×800 filled square from (0, 0) to (800, 800) in charstring space.
fn square_800_charstring() -> Vec<u8> {
    // 0 0 rmoveto, 800 hlineto, 800 vlineto, -800 hlineto, -800 vlineto, endchar
    // 800 = (249-247)*256 + 180 + 108 = 512 + 288 = 800
    // -800 = -(253-251)*256 - 180 - 108 = -800
    vec![
        139, 139, 21, // 0 0 rmoveto
        249, 180, 6, // 800 hlineto
        249, 180, 7, // 800 vlineto
        253, 180, 6, // -800 hlineto
        253, 180, 7,  // -800 vlineto
        14, // endchar
    ]
}

fn build_cid_cff_two_fds() -> Vec<u8> {
    let name_idx = encode_cff_index(&[b"PerFdTest"]);
    let string_idx = encode_cff_index(&[b"Adobe", b"Identity"]);
    let gsubr_idx = encode_cff_index(&[]);
    let notdef = vec![14u8];
    let sq = square_800_charstring();
    let charstrings_idx = encode_cff_index(&[&notdef, &sq, &sq]);
    let charset = vec![0u8, 0x00, 0x01, 0x00, 0x02];
    // FDSelect format 3: [0..2)->FD 0, [2..3)->FD 1, sentinel=3.
    let fd_select = vec![3u8, 0, 2, 0, 0, 0, 0, 2, 1, 0, 3];
    let private_dict = vec![139u8, 20];

    let build_fd = |mat: [&str; 6], priv_off: usize| -> Vec<u8> {
        let mut d = Vec::new();
        encode_cff_matrix(&mut d, &mat);
        encode_cff_int(&mut d, private_dict.len() as i32);
        encode_cff_int(&mut d, priv_off as i32);
        d.push(18);
        d
    };
    let build_top = |charset_off: usize, cs_off: usize, fd_arr_off: usize, fd_sel_off: usize| {
        let mut d = Vec::new();
        encode_cff_int(&mut d, 391);
        encode_cff_int(&mut d, 392);
        encode_cff_int(&mut d, 0);
        d.extend_from_slice(&[12, 30]);
        // Identity top matrix so each FD carries its own scaling matrix.
        encode_cff_matrix(&mut d, &["1", "0", "0", "1", "0", "0"]);
        encode_cff_int(&mut d, charset_off as i32);
        d.push(15);
        encode_cff_int(&mut d, cs_off as i32);
        d.push(17);
        encode_cff_int(&mut d, fd_arr_off as i32);
        d.extend_from_slice(&[12, 36]);
        encode_cff_int(&mut d, fd_sel_off as i32);
        d.extend_from_slice(&[12, 37]);
        d
    };

    let fd0_mat = ["0.001", "0", "0", "0.001", "0", "0"]; // 800-unit square -> 80pt at 100pt font size
    let fd1_mat = ["0.0005", "0", "0", "0.0005", "0", "0"]; // 800-unit square -> 40pt at 100pt font size

    let dummy_top = build_top(0, 0, 0, 0);
    let top_idx_len = encode_cff_index(&[&dummy_top]).len();
    let dummy_fd0 = build_fd(fd0_mat, 0);
    let dummy_fd1 = build_fd(fd1_mat, 0);
    let fd_array_len = encode_cff_index(&[&dummy_fd0, &dummy_fd1]).len();

    let charset_off = 4 + name_idx.len() + top_idx_len + string_idx.len() + gsubr_idx.len();
    let cs_off = charset_off + charset.len();
    let fd_sel_off = cs_off + charstrings_idx.len();
    let fd_arr_off = fd_sel_off + fd_select.len();
    let priv_off = fd_arr_off + fd_array_len;

    let top_idx = encode_cff_index(&[&build_top(charset_off, cs_off, fd_arr_off, fd_sel_off)]);
    let fd_array = encode_cff_index(&[&build_fd(fd0_mat, priv_off), &build_fd(fd1_mat, priv_off)]);

    let mut cff = vec![1u8, 0, 4, 1];
    cff.extend_from_slice(&name_idx);
    cff.extend_from_slice(&top_idx);
    cff.extend_from_slice(&string_idx);
    cff.extend_from_slice(&gsubr_idx);
    cff.extend_from_slice(&charset);
    cff.extend_from_slice(&charstrings_idx);
    cff.extend_from_slice(&fd_select);
    cff.extend_from_slice(&fd_array);
    cff.extend_from_slice(&private_dict);
    cff
}

#[test]
fn cid_font_type0c_renders_distinct_per_fd_font_matrices() {
    let cff = build_cid_cff_two_fds();
    // Draw CID 1 (FD 0 -> 80×80pt) at (20, 20), and CID 2 (FD 1 -> 40×40pt) at (140, 20).
    let content =
        b"0 0 0 rg BT /F1 100 Tf 1 0 0 1 20 20 Tm <0001> Tj 1 0 0 1 140 20 Tm <0002> Tj ET";
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 140] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /PerFdTest /Encoding /Identity-H \
          /DescendantFonts [6 0 R] >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType0 /BaseFont /PerFdTest \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /PerFdTest /Flags 4 \
          /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
          /CapHeight 700 /StemV 80 /FontFile3 8 0 R >>"
            .to_vec(),
        stream_obj("/Subtype /CIDFontType0C", &cff),
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&bytes);
    let rendered = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("render");

    let is_dark = |x: u32, pdf_y: u32| -> bool {
        let y = rendered.height - 1 - pdf_y;
        let off = ((y * rendered.width + x) * 4) as usize;
        rendered.data[off] < 30 && rendered.data[off + 1] < 30 && rendered.data[off + 2] < 30
    };

    // CID 1 (FD 0: 0.001 scale) covers [20..100] x [20..100].
    assert!(is_dark(50, 50), "CID 1 interior must be filled");
    assert!(is_dark(90, 90), "CID 1 upper-right (80x80) must be filled");
    assert!(!is_dark(110, 50), "outside CID 1 must stay white");

    // CID 2 (FD 1: 0.0005 scale) covers [140..180] x [20..60] — NOT [140..220] x [20..100]!
    assert!(is_dark(160, 40), "CID 2 scaled interior must be filled");
    assert!(
        !is_dark(200, 80),
        "CID 2 must be half-sized (40x40), not unscaled (80x80)"
    );
    assert!(!is_dark(160, 80), "CID 2 top must end at y=60, not y=100");
}
