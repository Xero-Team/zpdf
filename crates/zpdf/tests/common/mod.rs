//! Shared, byte-exact fixtures for integration tests.

/// Build a hand-written, fully valid PDF 1.7: catalog + page tree + one 612x792
/// page whose content shows "Hello". The classic xref-table offsets
/// (9 / 58 / 115 / 219, startxref 305) are computed to be correct; the
/// `assert_eq!(len == 305)` guards against any future body edit that desyncs
/// them. Doubles as the Phase-1 milestone fixture and as input for the
/// object-resolution caching / xref-recovery tests.
pub fn minimal_pdf() -> Vec<u8> {
    let offs = [9usize, 58, 115, 219]; // verified byte offsets of objs 1..=4
    let mut p = Vec::from(&b"%PDF-1.7\n"[..]);
    p.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    p.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    p.extend_from_slice(b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> /Contents 4 0 R >>\nendobj\n");
    p.extend_from_slice(
        b"4 0 obj\n<< /Length 36 >>\nstream\nBT /F1 24 Tf 72 720 Td (Hello) Tj ET\nendstream\nendobj\n",
    );
    assert_eq!(p.len(), 305, "xref must start at 305 — body length drifted");

    p.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for o in offs {
        p.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    p.extend_from_slice(b"trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n305\n%%EOF\n");
    p
}
