use std::sync::Arc;

use zpdf_font::system::SubstituteHints;
use zpdf_font::{FontProvider, FontProviderLimits};

const PRIMARY: &[u8] = include_bytes!("fixtures/coverage-primary.ttf");
const FALLBACK: &[u8] = include_bytes!("fixtures/coverage-fallback.ttf");
const BOLD: &[u8] = include_bytes!("fixtures/coverage-bold.ttf");

#[test]
fn style_selection_uses_metadata_and_ignores_subset_prefix_letters() {
    assert!(ttf_parser::Face::parse(BOLD, 0).unwrap().is_bold());
    let provider = FontProvider::memory_only();
    provider
        .register_with_aliases(BOLD, 0, &["SharedFamily"])
        .unwrap();
    provider
        .register_with_aliases(PRIMARY, 0, &["SharedFamily"])
        .unwrap();
    for name in ["SharedFamily", "BOLDAB+SharedFamily"] {
        let matched = provider
            .find_font(name, SubstituteHints::default(), None)
            .unwrap();
        assert_eq!(matched.data.as_ref(), PRIMARY);
    }
    let matched = provider
        .find_font(
            "SharedFamily",
            SubstituteHints {
                bold: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(matched.data.as_ref(), BOLD);
}

#[test]
fn registered_bytes_are_shared_and_outlive_the_caller_and_provider() {
    let bytes: Arc<[u8]> = Arc::from(PRIMARY);
    let provider = FontProvider::memory_only();
    let id = provider.register(Arc::clone(&bytes), 0).unwrap();
    let matched = provider
        .find_font(
            "ABCDEF+ZpdfCoveragePrimary",
            SubstituteHints::default(),
            None,
        )
        .unwrap();
    assert!(Arc::ptr_eq(&bytes, &matched.data));
    assert_eq!(provider.get(id).unwrap().face_index, 0);
    drop(bytes);
    drop(provider);
    let face = ttf_parser::Face::parse(&matched.data, matched.face_index).unwrap();
    assert!(face.glyph_index('中').is_some());
}

#[test]
fn duplicate_registration_is_idempotent_and_new_aliases_invalidate_misses() {
    let provider = FontProvider::memory_only();
    assert!(provider
        .find_font("ApplicationFace", SubstituteHints::default(), None)
        .is_none());
    let first = provider.register(PRIMARY, 0).unwrap();
    let generation = provider.generation();
    assert_eq!(provider.register(PRIMARY.to_vec(), 0).unwrap(), first);
    assert_eq!(provider.generation(), generation);
    assert_eq!(provider.bytes_used(), PRIMARY.len() as u64);
    assert_eq!(
        provider
            .register_with_aliases(PRIMARY, 0, &["ApplicationFace", "应用字体"])
            .unwrap(),
        first
    );
    assert!(provider.generation() > generation);
    for name in ["ApplicationFace", "应用字体", "ABCDEF+应用字体"] {
        assert!(provider
            .find_font(name, SubstituteHints::default(), None)
            .is_some());
    }
    assert_eq!(provider.len(), 1);
}

#[test]
fn malformed_faces_and_resource_limit_failures_are_atomic() {
    let provider = FontProvider::new(
        false,
        FontProviderLimits {
            max_font_bytes: PRIMARY.len() as u64,
            max_total_bytes: PRIMARY.len() as u64,
            max_faces: 1,
        },
    );
    assert!(provider.register(&b"not a font"[..], 0).is_err());
    assert!(provider.register(PRIMARY, 99).is_err());
    assert!(provider.register(vec![0; PRIMARY.len() + 1], 0).is_err());
    assert_eq!(provider.generation(), 0);
    assert_eq!(provider.bytes_used(), 0);
    provider.register(PRIMARY, 0).unwrap();
    let generation = provider.generation();
    assert!(provider.register(FALLBACK, 0).is_err());
    assert_eq!(provider.generation(), generation);
    assert_eq!(provider.len(), 1);
    let byte_limited = FontProvider::new(
        false,
        FontProviderLimits {
            max_total_bytes: PRIMARY.len() as u64 - 1,
            ..FontProviderLimits::default()
        },
    );
    assert!(byte_limited.register(PRIMARY, 0).is_err());
    assert!(byte_limited.is_empty());
}

#[test]
fn repeated_alias_registration_cannot_grow_metadata_without_bound() {
    let provider = FontProvider::memory_only();
    provider.register(PRIMARY, 0).unwrap();
    let mut rejected = false;
    for index in 0..256 {
        let alias = format!("Alias{index}");
        let generation = provider.generation();
        if provider
            .register_with_aliases(PRIMARY, 0, &[&alias])
            .is_err()
        {
            assert_eq!(provider.generation(), generation);
            assert!(provider
                .find_font(&alias, SubstituteHints::default(), None)
                .is_none());
            rejected = true;
            break;
        }
    }
    assert!(rejected, "metadata must have a cumulative bound");
}

/// TTC table offsets are relative to the whole collection, not each face.
fn collection() -> Arc<[u8]> {
    let mut bytes = b"ttcf\0\x01\0\0\0\0\0\x02\0\0\0\0\0\0\0\0".to_vec();
    for (index, font) in [PRIMARY, FALLBACK].iter().enumerate() {
        let start = bytes.len();
        bytes[12 + index * 4..16 + index * 4].copy_from_slice(&(start as u32).to_be_bytes());
        bytes.extend_from_slice(font);
        let tables = u16::from_be_bytes([font[4], font[5]]) as usize;
        for table in 0..tables {
            let offset_pos = start + 12 + table * 16 + 8;
            let relative =
                u32::from_be_bytes(bytes[offset_pos..offset_pos + 4].try_into().unwrap()) as usize;
            let absolute = start + relative;
            bytes[offset_pos..offset_pos + 4].copy_from_slice(&(absolute as u32).to_be_bytes());
            if &bytes[offset_pos - 8..offset_pos - 4] == b"head" {
                bytes[absolute + 8..absolute + 12].fill(0);
            }
        }
    }
    Arc::from(bytes)
}

#[test]
fn collection_faces_use_their_own_cmap_and_share_one_program_budget() {
    let bytes = collection();
    let provider = FontProvider::new(
        false,
        FontProviderLimits {
            max_total_bytes: bytes.len() as u64,
            ..FontProviderLimits::default()
        },
    );
    let first = provider.register(Arc::clone(&bytes), 0).unwrap();
    let second = provider.register(Arc::clone(&bytes), 1).unwrap();
    assert_ne!(first, second);
    assert_eq!(provider.bytes_used(), bytes.len() as u64);
    let primary = provider.get(first).unwrap();
    let fallback = provider.get(second).unwrap();
    assert!(Arc::ptr_eq(&primary.data, &fallback.data));
    assert!(ttf_parser::Face::parse(&primary.data, primary.face_index)
        .unwrap()
        .glyph_index('A')
        .is_some());
    let face = ttf_parser::Face::parse(&fallback.data, fallback.face_index).unwrap();
    assert!(face.glyph_index('A').is_none());
    assert!(face.glyph_index('你').is_some());
}

#[test]
fn concurrent_registration_keeps_one_identity() {
    let provider = Arc::new(FontProvider::memory_only());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let provider = Arc::clone(&provider);
            std::thread::spawn(move || provider.register(PRIMARY, 0).unwrap())
        })
        .collect();
    let ids: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(ids.iter().all(|id| *id == ids[0]));
    assert_eq!(provider.len(), 1);
    assert_eq!(provider.generation(), 1);
}
