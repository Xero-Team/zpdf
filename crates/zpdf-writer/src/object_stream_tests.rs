//! End-to-end crypt-filter coverage for compressed objects after an update.
use std::io::{Cursor, Write};

use zpdf_core::{ObjectId, ParseLimits, PdfDict, PdfName, PdfObject, PdfStream, PdfString};
use zpdf_parser::{PdfFile, XrefEntry};

use crate::encrypt::Encryptor;
use crate::serialize::{write_object, write_stream};
use crate::{EncryptionAlgorithm, EncryptionConfig, IncrementalWriter};

const CONTAINER: ObjectId = ObjectId(4, 0);
const MEMBER: ObjectId = ObjectId(5, 0);
const PAYLOAD: &[u8] = b"5 0 [(member string stays plaintext) 42]";

fn object_stream_dict() -> PdfDict {
    let mut dict = PdfDict::new();
    dict.insert(
        PdfName::new("Type"),
        PdfObject::Name(PdfName::new("ObjStm")),
    );
    dict.insert(PdfName::new("N"), PdfObject::Integer(1));
    dict.insert(PdfName::new("First"), PdfObject::Integer(4));
    dict
}

fn expected_member() -> PdfObject {
    PdfObject::Array(vec![
        PdfObject::String(PdfString::new(b"member string stays plaintext".to_vec())),
        PdfObject::Integer(42),
    ])
}

fn encrypted_object_stream(config: &EncryptionConfig) -> Vec<u8> {
    let file_id = b"object-stream-crypt-test";
    let encryptor = Encryptor::new(config, file_id).unwrap();
    let mut encryption = encryptor.encrypt_dict().clone();
    if config.algorithm == EncryptionAlgorithm::Rc4_128 {
        // R3 and R4 use the same password/key algorithms when metadata is
        // encrypted. Express the RC4 key as a V4/R4 named filter so /Crypt
        // selections below exercise a declared /CF entry, not a fallback.
        encryption.insert(PdfName::new("V"), PdfObject::Integer(4));
        encryption.insert(PdfName::new("R"), PdfObject::Integer(4));
        let mut stdcf = PdfDict::new();
        stdcf.insert(PdfName::new("CFM"), PdfObject::Name(PdfName::new("V2")));
        stdcf.insert(PdfName::new("Length"), PdfObject::Integer(16));
        let mut filters = PdfDict::new();
        filters.insert(PdfName::new("StdCF"), PdfObject::Dict(stdcf));
        encryption.insert(PdfName::new("CF"), PdfObject::Dict(filters));
        for key in ["StmF", "StrF"] {
            encryption.insert(PdfName::new(key), PdfObject::Name(PdfName::new("StdCF")));
        }
    }

    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = [0u64; 8];
    offsets[1] = pdf.len() as u64;
    pdf.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
    offsets[2] = pdf.len() as u64;
    pdf.extend_from_slice(b"2 0 obj << /Type /Pages /Count 1 /Kids [7 0 R] >> endobj\n");
    offsets[3] = pdf.len() as u64;
    write_object(&mut pdf, 3, 0, &PdfObject::Dict(encryption)).unwrap();
    offsets[4] = pdf.len() as u64;
    let encrypted = encryptor.try_encrypt_bytes(CONTAINER, PAYLOAD).unwrap();
    write_stream(&mut pdf, 4, 0, &object_stream_dict(), &encrypted).unwrap();
    offsets[7] = pdf.len() as u64;
    pdf.extend_from_slice(
        b"7 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >> endobj\n",
    );
    offsets[6] = pdf.len() as u64;

    let mut xref_data = Vec::new();
    for (num, offset) in offsets.into_iter().enumerate() {
        let (kind, field2, field3): (u8, u64, u16) = match num {
            0 => (0, 0, 65535),
            5 => (2, CONTAINER.0 as u64, 0),
            _ => (1, offset, 0),
        };
        xref_data.push(kind);
        xref_data.extend_from_slice(&field2.to_be_bytes());
        xref_data.extend_from_slice(&field3.to_be_bytes());
    }
    let mut xref = PdfDict::new();
    xref.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("XRef")));
    xref.insert(PdfName::new("Size"), PdfObject::Integer(8));
    xref.insert(PdfName::new("Root"), PdfObject::Ref(ObjectId(1, 0)));
    xref.insert(PdfName::new("Encrypt"), PdfObject::Ref(ObjectId(3, 0)));
    xref.insert(
        PdfName::new("W"),
        PdfObject::Array([1, 8, 2].into_iter().map(PdfObject::Integer).collect()),
    );
    let id = PdfObject::String(PdfString::new(file_id.to_vec()));
    xref.insert(PdfName::new("ID"), PdfObject::Array(vec![id.clone(), id]));
    write_stream(&mut pdf, 6, 0, &xref, &xref_data).unwrap();
    write!(pdf, "startxref\n{}\n%%EOF\n", offsets[6]).unwrap();
    pdf
}

#[test]
fn overwritten_object_stream_honors_crypt_filters() {
    for config in [
        EncryptionConfig::rc4_128("user", "owner"),
        EncryptionConfig::aes256("user", "owner"),
    ] {
        let original = encrypted_object_stream(&config);
        // The initial implicit cipher is a control: it must decode before the
        // edit as well as afterward, and member strings must decrypt only once.
        let initial = PdfFile::parse_with_password(original.clone(), b"user").unwrap();
        assert_eq!(initial.resolve(MEMBER).unwrap(), expected_member());
        for filter in [None, Some(""), Some("Identity"), Some("StdCF")] {
            for flate in [false, true] {
                let mut dict = object_stream_dict();
                let mut filters = Vec::new();
                let mut params = Vec::new();
                if let Some(name) = filter {
                    filters.push(PdfObject::Name(PdfName::new("Crypt")));
                    let mut crypt = PdfDict::new();
                    if !name.is_empty() {
                        crypt.insert(PdfName::new("Name"), PdfObject::Name(PdfName::new(name)));
                    }
                    params.push(PdfObject::Dict(crypt));
                }
                let data = if flate {
                    filters.push(PdfObject::Name(PdfName::new("FlateDecode")));
                    params.push(PdfObject::Null);
                    crate::flate_compress(PAYLOAD)
                } else {
                    PAYLOAD.to_vec()
                };
                if !filters.is_empty() {
                    let (filters, params, key) = if flate {
                        (PdfObject::Array(filters), PdfObject::Array(params), "DP")
                    } else {
                        (filters.remove(0), params.remove(0), "DecodeParms")
                    };
                    dict.insert(PdfName::new("Filter"), filters);
                    dict.insert(PdfName::new(key), params);
                }

                let mut writer =
                    IncrementalWriter::new_with_password(original.clone(), b"user").unwrap();
                writer.overwrite_object(CONTAINER, PdfObject::Stream(PdfStream::new(dict, data)));
                let mut output = Cursor::new(Vec::new());
                writer.write(&mut output).unwrap();
                let output = output.into_inner();
                assert!(output.starts_with(&original));
                assert_eq!(writer.document().file().data(), original);

                for cached in [false, true] {
                    for container_first in [false, true] {
                        let mut limits = ParseLimits::default();
                        if !cached {
                            limits.max_object_cache_bytes = 0;
                            limits.max_objstm_cache_bytes = 0;
                        }
                        let file = PdfFile::parse_with_password_and_limits(
                            output.clone(),
                            b"user",
                            limits,
                        )
                        .unwrap();
                        assert!(matches!(
                            file.xref.get(MEMBER),
                            Some(XrefEntry::Compressed { .. })
                        ));
                        if container_first {
                            assert_eq!(file.resolve_stream_data(CONTAINER).unwrap(), PAYLOAD);
                        }
                        for _ in 0..2 {
                            let member = file.resolve(MEMBER).unwrap_or_else(|error| {
                                panic!("compressed member: {:?}, filter={filter:?}, flate={flate}, cached={cached}, container_first={container_first}: {error}", config.algorithm)
                            });
                            assert_eq!(member, expected_member());
                        }
                        assert_eq!(file.resolve_stream_data(CONTAINER).unwrap(), PAYLOAD);
                    }
                }
            }
        }
    }
}
