//! PDF object serialization to text format (ISO 32000-1 §7.3).

use std::io::{self, Write};

use zpdf_core::{PdfDict, PdfName, PdfObject};

/// Serialize an indirect object in PDF syntax.
/// Format: `<num> <gen> obj\n<content>\nendobj\n`
#[cfg(test)]
pub fn serialize_object(num: u32, gen: u32, obj: &PdfObject) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    write_object(&mut buf, num, gen, obj)?;
    Ok(buf)
}

/// Write an indirect object directly to an output without staging a second
/// full-sized buffer.
pub fn write_object<W: Write>(out: &mut W, num: u32, gen: u32, obj: &PdfObject) -> io::Result<()> {
    writeln!(out, "{num} {gen} obj")?;
    serialize_direct_object(out, obj)?;
    out.write_all(b"\nendobj\n")
}

/// Serialize a stream object (dict + binary data).
/// Format: `<num> <gen> obj\n<< /Length <n> ... >> stream\n<data>\nendstream\nendobj\n`
#[cfg(test)]
pub fn serialize_stream(num: u32, gen: u32, dict: &PdfDict, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    write_stream(&mut buf, num, gen, dict, data)?;
    Ok(buf)
}

/// Write a stream directly to an output, keeping the potentially large stream
/// payload zero-copy.
pub fn write_stream<W: Write>(
    out: &mut W,
    num: u32,
    gen: u32,
    dict: &PdfDict,
    data: &[u8],
) -> io::Result<()> {
    writeln!(out, "{num} {gen} obj")?;

    // Add /Length to the dict.
    let mut dict = dict.clone();
    let length = i64::try_from(data.len())
        .map_err(|_| invalid_input("stream length exceeds PDF integer range"))?;
    dict.insert(PdfName("Length".to_string()), PdfObject::Integer(length));

    serialize_dict(out, &dict)?;
    out.write_all(b"\nstream\n")?;
    out.write_all(data)?;
    out.write_all(b"\nendstream\nendobj\n")
}

/// Serialize a direct PDF object (not wrapped in `obj`/`endobj`).
fn serialize_direct_object<W: Write>(buf: &mut W, obj: &PdfObject) -> io::Result<()> {
    match obj {
        PdfObject::Null => buf.write_all(b"null")?,
        PdfObject::Bool(b) => buf.write_all(if *b { b"true" } else { b"false" })?,
        PdfObject::Integer(n) => write!(buf, "{n}")?,
        PdfObject::Real(f) => {
            if !f.is_finite() {
                return Err(invalid_input("PDF real numbers must be finite"));
            }
            // Format floats with up to 6 decimal places, stripping trailing zeros.
            let s = format!("{:.6}", f);
            let trimmed = s.trim_end_matches('0').trim_end_matches('.');
            buf.write_all(trimmed.as_bytes())?;
        }
        PdfObject::String(s) => {
            // Literal string: (escaped)
            buf.write_all(b"(")?;
            for &b in s.as_bytes() {
                match b {
                    b'(' | b')' | b'\\' => {
                        buf.write_all(&[b'\\', b])?;
                    }
                    b'\n' => buf.write_all(b"\\n")?,
                    b'\r' => buf.write_all(b"\\r")?,
                    b'\t' => buf.write_all(b"\\t")?,
                    _ => buf.write_all(&[b])?,
                }
            }
            buf.write_all(b")")?;
        }
        PdfObject::Name(n) => serialize_name(buf, n.as_str())?,
        PdfObject::Array(arr) => {
            buf.write_all(b"[")?;
            for (i, elem) in arr.iter().enumerate() {
                if i > 0 {
                    buf.write_all(b" ")?;
                }
                serialize_direct_object(buf, elem)?;
            }
            buf.write_all(b"]")?;
        }
        PdfObject::Dict(dict) => {
            serialize_dict(buf, dict)?;
        }
        PdfObject::Ref(r) => {
            write!(buf, "{} {} R", r.0, r.1)?;
        }
        PdfObject::Stream(_) => {
            // Streams must be serialized with serialize_stream, not as direct objects.
            return Err(invalid_input(
                "cannot serialize a stream as a direct object",
            ));
        }
    }
    Ok(())
}

/// Serialize a name token (`/Name`), escaping special characters as `#XX`.
fn serialize_name<W: Write>(buf: &mut W, name: &str) -> io::Result<()> {
    buf.write_all(b"/")?;
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' {
            buf.write_all(&[b])?;
        } else {
            write!(buf, "#{b:02X}")?;
        }
    }
    Ok(())
}

/// Serialize a dictionary.
pub fn serialize_dict<W: Write>(buf: &mut W, dict: &PdfDict) -> io::Result<()> {
    buf.write_all(b"<< ")?;
    for (key, value) in &dict.0 {
        serialize_name(buf, key.as_str())?;
        buf.write_all(b" ")?;
        serialize_direct_object(buf, value)?;
        buf.write_all(b" ")?;
    }
    buf.write_all(b">>")
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::ObjectId;

    #[test]
    fn serialize_simple_object() {
        let obj = PdfObject::Integer(42);
        let bytes = serialize_object(5, 0, &obj).unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "5 0 obj\n42\nendobj\n");
    }

    #[test]
    fn serialize_dict_with_ref() {
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName("Type".to_string()),
            PdfObject::Name(PdfName("Page".to_string())),
        );
        dict.insert(
            PdfName("Parent".to_string()),
            PdfObject::Ref(ObjectId(3, 0)),
        );
        let bytes = serialize_object(10, 0, &PdfObject::Dict(dict)).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("10 0 obj"));
        assert!(s.contains("/Type /Page"));
        assert!(s.contains("/Parent 3 0 R"));
        assert!(s.contains("endobj"));
    }

    #[test]
    fn serialize_array() {
        let arr = vec![
            PdfObject::Integer(1),
            PdfObject::Real(2.5),
            PdfObject::Name(PdfName("Foo".to_string())),
        ];
        let bytes = serialize_object(7, 0, &PdfObject::Array(arr)).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("[1 2.5 /Foo]"));
    }

    #[test]
    fn serialize_string_escapes() {
        let obj = PdfObject::String(zpdf_core::PdfString(b"Hello (world)\n".to_vec()));
        let bytes = serialize_object(2, 0, &obj).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains(r"(Hello \(world\)\n)"));
    }

    #[test]
    fn serialize_name_escapes() {
        let obj = PdfObject::Name(PdfName("Foo#Bar Baz".to_string()));
        let bytes = serialize_object(3, 0, &obj).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        // '#' -> #23, space -> #20
        assert!(s.contains("/Foo#23Bar#20Baz"));
    }

    #[test]
    fn serialize_stream_with_length() {
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName("Type".to_string()),
            PdfObject::Name(PdfName("XObject".to_string())),
        );
        let data = b"q 1 0 0 1 0 0 cm Q";
        let bytes = serialize_stream(15, 0, &dict, data).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("15 0 obj"));
        assert!(s.contains("/Length 18"));
        assert!(s.contains("stream\nq 1 0 0 1 0 0 cm Q\nendstream"));
    }

    #[test]
    fn real_strips_trailing_zeros() {
        let obj = PdfObject::Real(1.500000);
        let mut buf = Vec::new();
        serialize_direct_object(&mut buf, &obj).unwrap();
        assert_eq!(String::from_utf8_lossy(&buf), "1.5");
    }

    #[test]
    fn nested_stream_is_an_error_instead_of_a_panic() {
        let stream = zpdf_core::PdfStream::new(PdfDict::new(), b"data".to_vec());
        let obj = PdfObject::Array(vec![PdfObject::Stream(stream)]);
        let err = serialize_object(1, 0, &obj).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn non_finite_real_is_rejected() {
        let err = serialize_object(1, 0, &PdfObject::Real(f64::NAN)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
