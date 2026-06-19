use crate::ObjectId;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a PDF file")]
    NotAPdf,

    #[error("unsupported PDF version: {0}.{1}")]
    UnsupportedVersion(u8, u8),

    #[error("unexpected end of file at offset {0}")]
    UnexpectedEof(u64),

    #[error("invalid xref table at offset {0}")]
    InvalidXref(u64),

    #[error("object not found: {0}")]
    ObjectNotFound(ObjectId),

    #[error("type mismatch: expected {expected}, got {actual}")]
    TypeMismatch {
        expected: &'static str,
        actual: &'static str,
    },

    #[error("missing required key: /{0}")]
    MissingKey(String),

    #[error("invalid object at offset {0}: {1}")]
    InvalidObject(u64, String),

    #[error("stream decode error: {0}")]
    StreamDecode(String),

    #[error("unsupported filter: {0}")]
    UnsupportedFilter(String),

    #[error("incorrect password for an encrypted document")]
    WrongPassword,

    #[error("recursion depth exceeded (max {0})")]
    RecursionLimit(u32),

    #[error("stream size exceeded (max {0} bytes)")]
    StreamSizeLimit(u64),

    #[error("string length exceeded (max {0} bytes)")]
    StringLengthLimit(u32),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
