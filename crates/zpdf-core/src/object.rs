use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::{Error, Rect, Result};

/// Indirect object identifier: (object number, generation number).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u32, pub u16);

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} R", self.0, self.1)
    }
}

/// PDF Name object (e.g., `/Type` stored as `"Type"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PdfName(pub String);

impl PdfName {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for PdfName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for PdfName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.0)
    }
}

/// PDF string (literal or hexadecimal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfString(pub Vec<u8>);

impl PdfString {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

/// PDF dictionary: ordered map of Name → PdfObject.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfDict(pub BTreeMap<PdfName, PdfObject>);

impl PdfDict {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn get(&self, key: &str) -> Option<&PdfObject> {
        self.0.get(key)
    }

    pub fn insert(&mut self, key: PdfName, value: PdfObject) {
        self.0.insert(key, value);
    }

    pub fn get_name(&self, key: &str) -> Result<&str> {
        match self.get(key) {
            Some(PdfObject::Name(n)) => Ok(n.as_str()),
            Some(other) => Err(Error::TypeMismatch {
                expected: "Name",
                actual: other.type_name(),
            }),
            None => Err(Error::MissingKey(key.to_string())),
        }
    }

    pub fn get_i64(&self, key: &str) -> Result<i64> {
        match self.get(key) {
            Some(PdfObject::Integer(n)) => Ok(*n),
            Some(other) => Err(Error::TypeMismatch {
                expected: "Integer",
                actual: other.type_name(),
            }),
            None => Err(Error::MissingKey(key.to_string())),
        }
    }

    pub fn get_f64(&self, key: &str) -> Result<f64> {
        match self.get(key) {
            Some(PdfObject::Real(n)) => Ok(*n),
            Some(PdfObject::Integer(n)) => Ok(*n as f64),
            Some(other) => Err(Error::TypeMismatch {
                expected: "number",
                actual: other.type_name(),
            }),
            None => Err(Error::MissingKey(key.to_string())),
        }
    }

    pub fn get_array(&self, key: &str) -> Result<&[PdfObject]> {
        match self.get(key) {
            Some(PdfObject::Array(a)) => Ok(a),
            Some(other) => Err(Error::TypeMismatch {
                expected: "Array",
                actual: other.type_name(),
            }),
            None => Err(Error::MissingKey(key.to_string())),
        }
    }

    pub fn get_dict(&self, key: &str) -> Result<&PdfDict> {
        match self.get(key) {
            Some(PdfObject::Dict(d)) => Ok(d),
            Some(other) => Err(Error::TypeMismatch {
                expected: "Dict",
                actual: other.type_name(),
            }),
            None => Err(Error::MissingKey(key.to_string())),
        }
    }

    pub fn get_ref(&self, key: &str) -> Result<ObjectId> {
        match self.get(key) {
            Some(PdfObject::Ref(id)) => Ok(*id),
            Some(other) => Err(Error::TypeMismatch {
                expected: "Ref",
                actual: other.type_name(),
            }),
            None => Err(Error::MissingKey(key.to_string())),
        }
    }

    pub fn get_rect(&self, key: &str) -> Result<Rect> {
        let arr = self.get_array(key)?;
        if arr.len() != 4 {
            return Err(Error::TypeMismatch {
                expected: "4-element array (Rect)",
                actual: "wrong-length array",
            });
        }
        let to_f64 = |obj: &PdfObject| -> Result<f64> {
            match obj {
                PdfObject::Real(n) => Ok(*n),
                PdfObject::Integer(n) => Ok(*n as f64),
                _ => Err(Error::TypeMismatch {
                    expected: "number",
                    actual: obj.type_name(),
                }),
            }
        };
        Ok(Rect::new(
            to_f64(&arr[0])?,
            to_f64(&arr[1])?,
            to_f64(&arr[2])?,
            to_f64(&arr[3])?,
        ))
    }
}

impl Default for PdfDict {
    fn default() -> Self {
        Self::new()
    }
}

/// PDF stream object: dictionary + raw byte range (lazy decode).
#[derive(Debug, Clone, PartialEq)]
pub struct PdfStream {
    pub dict: PdfDict,
    pub data: Arc<[u8]>,
}

impl PdfStream {
    pub fn new(dict: PdfDict, data: Vec<u8>) -> Self {
        Self {
            dict,
            data: data.into(),
        }
    }
}

/// All PDF object types.
#[derive(Debug, Clone, PartialEq)]
pub enum PdfObject {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    String(PdfString),
    Name(PdfName),
    Array(Vec<PdfObject>),
    Dict(PdfDict),
    Stream(PdfStream),
    Ref(ObjectId),
}

impl PdfObject {
    pub fn type_name(&self) -> &'static str {
        match self {
            PdfObject::Null => "Null",
            PdfObject::Bool(_) => "Bool",
            PdfObject::Integer(_) => "Integer",
            PdfObject::Real(_) => "Real",
            PdfObject::String(_) => "String",
            PdfObject::Name(_) => "Name",
            PdfObject::Array(_) => "Array",
            PdfObject::Dict(_) => "Dict",
            PdfObject::Stream(_) => "Stream",
            PdfObject::Ref(_) => "Ref",
        }
    }

    pub fn as_i64(&self) -> Result<i64> {
        match self {
            PdfObject::Integer(n) => Ok(*n),
            _ => Err(Error::TypeMismatch {
                expected: "Integer",
                actual: self.type_name(),
            }),
        }
    }

    pub fn as_f64(&self) -> Result<f64> {
        match self {
            PdfObject::Real(n) => Ok(*n),
            PdfObject::Integer(n) => Ok(*n as f64),
            _ => Err(Error::TypeMismatch {
                expected: "number",
                actual: self.type_name(),
            }),
        }
    }

    pub fn as_name(&self) -> Result<&str> {
        match self {
            PdfObject::Name(n) => Ok(n.as_str()),
            _ => Err(Error::TypeMismatch {
                expected: "Name",
                actual: self.type_name(),
            }),
        }
    }

    pub fn as_str(&self) -> Result<&PdfString> {
        match self {
            PdfObject::String(s) => Ok(s),
            _ => Err(Error::TypeMismatch {
                expected: "String",
                actual: self.type_name(),
            }),
        }
    }

    pub fn as_array(&self) -> Result<&[PdfObject]> {
        match self {
            PdfObject::Array(a) => Ok(a),
            _ => Err(Error::TypeMismatch {
                expected: "Array",
                actual: self.type_name(),
            }),
        }
    }

    pub fn as_dict(&self) -> Result<&PdfDict> {
        match self {
            PdfObject::Dict(d) => Ok(d),
            _ => Err(Error::TypeMismatch {
                expected: "Dict",
                actual: self.type_name(),
            }),
        }
    }

    pub fn as_stream(&self) -> Result<&PdfStream> {
        match self {
            PdfObject::Stream(s) => Ok(s),
            _ => Err(Error::TypeMismatch {
                expected: "Stream",
                actual: self.type_name(),
            }),
        }
    }

    pub fn as_ref(&self) -> Result<ObjectId> {
        match self {
            PdfObject::Ref(id) => Ok(*id),
            _ => Err(Error::TypeMismatch {
                expected: "Ref",
                actual: self.type_name(),
            }),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, PdfObject::Null)
    }
}

impl fmt::Display for PdfObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdfObject::Null => write!(f, "null"),
            PdfObject::Bool(b) => write!(f, "{}", if *b { "true" } else { "false" }),
            PdfObject::Integer(n) => write!(f, "{n}"),
            PdfObject::Real(n) => write!(f, "{n}"),
            PdfObject::String(s) => write!(f, "({})", s.to_string_lossy()),
            PdfObject::Name(n) => write!(f, "{n}"),
            PdfObject::Array(a) => {
                write!(f, "[")?;
                for (i, obj) in a.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{obj}")?;
                }
                write!(f, "]")
            }
            PdfObject::Dict(d) => {
                write!(f, "<< ")?;
                for (k, v) in &d.0 {
                    write!(f, "{k} {v} ")?;
                }
                write!(f, ">>")
            }
            PdfObject::Stream(s) => {
                write!(f, "<< ")?;
                for (k, v) in &s.dict.0 {
                    write!(f, "{k} {v} ")?;
                }
                write!(f, ">> stream({} bytes)", s.data.len())
            }
            PdfObject::Ref(id) => write!(f, "{id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_accessors() {
        let mut d = PdfDict::new();
        d.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Page")));
        d.insert(PdfName::new("Count"), PdfObject::Integer(5));

        assert_eq!(d.get_name("Type").unwrap(), "Page");
        assert_eq!(d.get_i64("Count").unwrap(), 5);
        assert!(d.get_name("Missing").is_err());
    }

    #[test]
    fn dict_get_rect() {
        let mut d = PdfDict::new();
        d.insert(
            PdfName::new("MediaBox"),
            PdfObject::Array(vec![
                PdfObject::Integer(0),
                PdfObject::Integer(0),
                PdfObject::Real(612.0),
                PdfObject::Real(792.0),
            ]),
        );
        let r = d.get_rect("MediaBox").unwrap();
        assert!((r.x1 - 612.0).abs() < 1e-10);
        assert!((r.y1 - 792.0).abs() < 1e-10);
    }

    #[test]
    fn object_display() {
        let obj = PdfObject::Dict(PdfDict::new());
        assert_eq!(format!("{obj}"), "<< >>");

        let obj = PdfObject::Ref(ObjectId(12, 0));
        assert_eq!(format!("{obj}"), "12 0 R");
    }
}
