//! PDF object serialization to text format (ISO 32000-1 §7.3).

use zpdf_core::{PdfDict, PdfName, PdfObject};

/// Serialize an indirect object in PDF syntax.
/// Format: `<num> <gen> obj\n<content>\nendobj\n`
pub fn serialize_object(num: u32, gen: u32, obj: &PdfObject) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("{} {} obj\n", num, gen).as_bytes());
    serialize_direct_object(&mut buf, obj);
    buf.extend_from_slice(b"\nendobj\n");
    buf
}

/// Serialize a stream object (dict + binary data).
/// Format: `<num> <gen> obj\n<< /Length <n> ... >> stream\n<data>\nendstream\nendobj\n`
pub fn serialize_stream(num: u32, gen: u32, dict: &PdfDict, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("{} {} obj\n", num, gen).as_bytes());

    // Add /Length to the dict.
    let mut dict = dict.clone();
    dict.insert(
        PdfName("Length".to_string()),
        PdfObject::Integer(data.len() as i64),
    );

    serialize_dict(&mut buf, &dict);
    buf.extend_from_slice(b"\nstream\n");
    buf.extend_from_slice(data);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    buf
}

/// Serialize a direct PDF object (not wrapped in `obj`/`endobj`).
fn serialize_direct_object(buf: &mut Vec<u8>, obj: &PdfObject) {
    match obj {
        PdfObject::Null => buf.extend_from_slice(b"null"),
        PdfObject::Bool(b) => buf.extend_from_slice(if *b { b"true" } else { b"false" }),
        PdfObject::Integer(n) => buf.extend_from_slice(format!("{}", n).as_bytes()),
        PdfObject::Real(f) => {
            // Format floats with up to 6 decimal places, stripping trailing zeros.
            let s = format!("{:.6}", f);
            let trimmed = s.trim_end_matches('0').trim_end_matches('.');
            buf.extend_from_slice(trimmed.as_bytes());
        }
        PdfObject::String(s) => {
            // Literal string: (escaped)
            buf.push(b'(');
            for &b in s.as_bytes() {
                match b {
                    b'(' | b')' | b'\\' => {
                        buf.push(b'\\');
                        buf.push(b);
                    }
                    b'\n' => buf.extend_from_slice(b"\\n"),
                    b'\r' => buf.extend_from_slice(b"\\r"),
                    b'\t' => buf.extend_from_slice(b"\\t"),
                    _ => buf.push(b),
                }
            }
            buf.push(b')');
        }
        PdfObject::Name(n) => {
            buf.push(b'/');
            // Names must escape special characters as #XX.
            for &b in n.as_str().as_bytes() {
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' {
                    buf.push(b);
                } else {
                    buf.extend_from_slice(format!("#{:02X}", b).as_bytes());
                }
            }
        }
        PdfObject::Array(arr) => {
            buf.push(b'[');
            for (i, elem) in arr.iter().enumerate() {
                if i > 0 {
                    buf.push(b' ');
                }
                serialize_direct_object(buf, elem);
            }
            buf.push(b']');
        }
        PdfObject::Dict(dict) => {
            serialize_dict(buf, dict);
        }
        PdfObject::Ref(r) => {
            buf.extend_from_slice(format!("{} {} R", r.0, r.1).as_bytes());
        }
        PdfObject::Stream(_) => {
            // Streams must be serialized with serialize_stream, not as direct objects.
            panic!("cannot serialize stream as direct object");
        }
    }
}

/// Serialize a dictionary.
pub fn serialize_dict(buf: &mut Vec<u8>, dict: &PdfDict) {
    buf.extend_from_slice(b"<< ");
    for (key, value) in &dict.0 {
        buf.push(b'/');
        buf.extend_from_slice(key.as_str().as_bytes());
        buf.push(b' ');
        serialize_direct_object(buf, value);
        buf.push(b' ');
    }
    buf.extend_from_slice(b">>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::ObjectId;

    #[test]
    fn serialize_simple_object() {
        let obj = PdfObject::Integer(42);
        let bytes = serialize_object(5, 0, &obj);
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
        let bytes = serialize_object(10, 0, &PdfObject::Dict(dict));
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
        let bytes = serialize_object(7, 0, &PdfObject::Array(arr));
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("[1 2.5 /Foo]"));
    }

    #[test]
    fn serialize_string_escapes() {
        let obj = PdfObject::String(zpdf_core::PdfString(b"Hello (world)\n".to_vec()));
        let bytes = serialize_object(2, 0, &obj);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains(r"(Hello \(world\)\n)"));
    }

    #[test]
    fn serialize_name_escapes() {
        let obj = PdfObject::Name(PdfName("Foo#Bar Baz".to_string()));
        let bytes = serialize_object(3, 0, &obj);
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
        let bytes = serialize_stream(15, 0, &dict, data);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("15 0 obj"));
        assert!(s.contains("/Length 18"));
        assert!(s.contains("stream\nq 1 0 0 1 0 0 cm Q\nendstream"));
    }

    #[test]
    fn real_strips_trailing_zeros() {
        let obj = PdfObject::Real(1.500000);
        let mut buf = Vec::new();
        serialize_direct_object(&mut buf, &obj);
        assert_eq!(String::from_utf8_lossy(&buf), "1.5");
    }
}
