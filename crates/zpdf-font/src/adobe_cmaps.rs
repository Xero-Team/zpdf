//! Shared static mappings generated from pinned, licensed Adobe resources.
//! All reads are bounds checked; no I/O, decompression or per-map allocation.

const DATA: &[u8] = include_bytes!("../data/adobe-cmaps/maps.bin");
const TEXT: &[u8] = include_bytes!("../data/adobe-cmaps/text.bin");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharacterCollection {
    Cns1,
    Gb1,
    Japan1,
    Korea1,
    Kr,
}

impl CharacterCollection {
    pub fn from_ordering(ordering: &str) -> Option<Self> {
        match ordering {
            "CNS1" => Some(Self::Cns1),
            "GB1" => Some(Self::Gb1),
            "Japan1" => Some(Self::Japan1),
            "Korea1" => Some(Self::Korea1),
            "KR" => Some(Self::Kr),
            _ => None,
        }
    }

    pub fn unicode(self, cid: u32) -> Option<CMapUnicode> {
        let (_, offset, count) = COLLECTIONS
            .iter()
            .find(|(collection, _, _)| *collection == self)?;
        if cid as usize >= *count {
            return None;
        }
        decode_unicode(read_u32(offset + cid as usize * 4)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CMapUnicode {
    Scalar(char),
    Sequence(&'static str),
}

impl CMapUnicode {
    pub fn scalar(self) -> Option<u32> {
        match self {
            Self::Scalar(ch) => Some(ch as u32),
            Self::Sequence(text) => {
                let mut chars = text.chars();
                let first = chars.next()?;
                chars.next().is_none().then_some(first as u32)
            }
        }
    }

    pub fn push_to(self, text: &mut String) {
        match self {
            Self::Scalar(ch) => text.push(ch),
            Self::Sequence(value) => text.push_str(value),
        }
    }

    pub(crate) fn font_character(self) -> Option<crate::FontCharacter> {
        match self {
            Self::Scalar(character) => Some(crate::FontCharacter {
                character,
                variation_selector: None,
            }),
            Self::Sequence(text) => crate::FontCharacter::from_text(text),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceEncoding {
    Legacy,
    Ucs2,
    Utf16,
}

#[derive(Debug)]
pub(crate) struct PredefinedMap {
    pub name: &'static str,
    pub collection: CharacterCollection,
    pub wmode: u8,
    pub encoding: SourceEncoding,
    pub codespaces: &'static [(u8, u32, u32)],
    groups: [(usize, usize, bool, usize); 4],
}

include!("../data/adobe-cmaps/index.rs");

pub(crate) fn predefined(name: &str) -> Option<&'static PredefinedMap> {
    MAPS.binary_search_by_key(&name, |map| map.name)
        .ok()
        .map(|index| &MAPS[index])
}

fn read_u32(offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        DATA.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
fn read_u16(offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        DATA.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn decode_unicode(value: u32) -> Option<CMapUnicode> {
    if value == u32::MAX {
        return None;
    }
    if value & 0x80000000 == 0 {
        return char::from_u32(value).map(CMapUnicode::Scalar);
    }
    let offset = (value & 0x7FFFFFFF) as usize;
    let length = u16::from_le_bytes(TEXT.get(offset..offset + 2)?.try_into().ok()?) as usize;
    std::str::from_utf8(TEXT.get(offset + 2..offset + 2 + length)?)
        .ok()
        .map(CMapUnicode::Sequence)
}

impl PredefinedMap {
    /// Unicode-source semantics remain useful for extraction and recovery
    /// even when this collection has no CID for the encoded character.
    pub fn source_unicode(&self, code: u32, length: u8) -> Option<CMapUnicode> {
        let scalar = match (self.encoding, length) {
            (SourceEncoding::Ucs2 | SourceEncoding::Utf16, 2) if code <= 0xFFFF => code,
            (SourceEncoding::Utf16, 4) if (0x10000..=0x10FFFF).contains(&code) => code,
            (SourceEncoding::Utf16, 4) => {
                let (high, low) = (code >> 16, code & 0xFFFF);
                if !(0xD800..=0xDBFF).contains(&high) || !(0xDC00..=0xDFFF).contains(&low) {
                    return None;
                }
                0x10000 + ((high - 0xD800) << 10) + low - 0xDC00
            }
            _ => return None,
        };
        char::from_u32(scalar).map(CMapUnicode::Scalar)
    }

    pub fn lookup(&self, code: u32, length: u8) -> Option<CMapUnicode> {
        decode_unicode(self.lookup_value(code, length)?.1)
    }

    pub fn lookup_cid(&self, code: u32, length: u8) -> Option<u16> {
        Some(self.lookup_value(code, length)?.0)
    }

    pub fn lookup_scalar(&self, code: u32, length: u8) -> Option<u32> {
        let value = self.lookup_value(code, length)?.1;
        char::from_u32(value).map(|ch| ch as u32)
    }

    fn lookup_value(&self, mut code: u32, length: u8) -> Option<(u16, u32)> {
        if !(1..=4).contains(&length) {
            return None;
        }
        // next_code exposes a scalar for UTF-16, but Adobe's keys retain bytes.
        if self.encoding == SourceEncoding::Utf16
            && length == 4
            && (0x10000..=0x10FFFF).contains(&code)
        {
            let value = code - 0x10000;
            code = ((0xD800 + (value >> 10)) << 16) | (0xDC00 + (value & 1023));
        }
        let (offset, count, direct, bucket) = self.groups[length as usize - 1];
        let prefix = code >> 8;
        let descriptor = if direct {
            if prefix as usize >= count {
                return None;
            }
            read_u32(offset + prefix as usize * 4)?
        } else {
            let (mut low, mut high) = if bucket != 0 {
                let position = bucket - 1 + (code >> 24) as usize * 4;
                (
                    read_u32(position)? as usize,
                    read_u32(position + 4)? as usize,
                )
            } else {
                (0, count)
            };
            let end = high;
            while low < high {
                let mid = low + (high - low) / 2;
                if read_u32(offset + mid * 8)? < prefix {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            if low == end || read_u32(offset + low * 8)? != prefix {
                return None;
            }
            read_u32(offset + low * 8 + 4)?
        };
        if descriptor == u32::MAX {
            return None;
        }
        let page = (descriptor & 0x3FFFFFFF) as usize;
        let implicit = descriptor & 0x40000000 != 0;
        let last = (code & 255) as usize;
        let position = if descriptor & 0x80000000 != 0 {
            let count = read_u16(page)? as usize;
            let stride = if implicit { 3 } else { 7 };
            let (mut low, mut high) = (0, count);
            while low < high {
                let mid = low + (high - low) / 2;
                if (*DATA.get(page + 2 + mid * stride)? as usize) < last {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            if low == count || *DATA.get(page + 2 + low * stride)? as usize != last {
                return None;
            }
            page + 3 + low * stride
        } else {
            page + last * if implicit { 2 } else { 6 }
        };
        let cid = read_u16(position)?;
        let unicode = if implicit {
            if cid == u16::MAX {
                return None;
            }
            if self.encoding == SourceEncoding::Utf16 && length == 4 {
                let (high, low) = (code >> 16, code & 0xFFFF);
                if !(0xD800..=0xDBFF).contains(&high) || !(0xDC00..=0xDFFF).contains(&low) {
                    return None;
                }
                0x10000 + ((high - 0xD800) << 10) + low - 0xDC00
            } else {
                code
            }
        } else {
            let value = read_u32(position + 2)?;
            if cid == 0 && value == u32::MAX {
                return None;
            }
            value
        };
        Some((cid, unicode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("../data/adobe-cmaps/test-oracle.rs");

    fn add(hash: &mut u64, length: u8, code: u32, cid: u16, unicode: Option<CMapUnicode>) {
        let mut text = String::new();
        if let Some(value) = unicode {
            value.push_to(&mut text);
        }
        for byte in [length]
            .into_iter()
            .chain(code.to_be_bytes())
            .chain(cid.to_be_bytes())
            .chain((text.len() as u16).to_be_bytes())
            .chain(text.bytes())
        {
            *hash = (*hash ^ byte as u64).wrapping_mul(0x100000001B3);
        }
    }

    #[test]
    fn every_generated_mapping_matches_the_source_oracle() {
        assert_eq!(MAPS.len(), MAP_ORACLE.len());
        for &(name, expected_count, expected_hash) in MAP_ORACLE {
            let map = predefined(name).unwrap();
            let (mut count, mut hash) = (0, 0xCBF29CE484222325);
            for (group, &(offset, pages, direct, _)) in map.groups.iter().enumerate() {
                for page in 0..pages {
                    let prefix = if direct {
                        page as u32
                    } else {
                        read_u32(offset + page * 8).unwrap()
                    };
                    for low in 0..256 {
                        let code = (prefix << 8) | low;
                        if let Some((cid, value)) = map.lookup_value(code, group as u8 + 1) {
                            add(&mut hash, group as u8 + 1, code, cid, decode_unicode(value));
                            count += 1;
                        }
                    }
                }
            }
            assert_eq!((count, hash), (expected_count, expected_hash), "{name}");
            assert!(map.lookup(0, 0).is_none());
            assert!(map.lookup(0, 5).is_none());
            assert!(map.lookup(u32::MAX, 4).is_none());
        }
    }

    #[test]
    fn every_canonical_string_matches_the_source_oracle() {
        for &(ordering, expected_count, expected_hash) in COLLECTION_ORACLE {
            let collection = CharacterCollection::from_ordering(ordering).unwrap();
            let (mut count, mut hash) = (0, 0xCBF29CE484222325);
            for cid in 0..=u16::MAX {
                if let Some(text) = collection.unicode(cid as u32) {
                    add(&mut hash, 2, cid as u32, cid, Some(text));
                    count += 1;
                }
            }
            assert_eq!((count, hash), (expected_count, expected_hash), "{ordering}");
            assert!(collection.unicode(u32::MAX).is_none());
        }
    }
}
