use flate2::{write::ZlibEncoder, Compression};
use std::io::Write;
use std::sync::Arc;
use zpdf_font::system::SubstituteHints;
use zpdf_font::{FontPack, FontPackFormat, FontPackLimits, FontProvider, FontProviderLimits};

const FONT: &[u8] = include_bytes!("fixtures/coverage-primary.ttf");

fn compressed(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn pack(data: Vec<u8>, limit: u64) -> Arc<FontPack> {
    Arc::new(
        FontPack::new(
            data,
            FontPackFormat::Zlib,
            FontPackLimits {
                max_decoded_bytes: limit,
                ..Default::default()
            },
        )
        .unwrap(),
    )
}

#[test]
fn decode_is_lazy_shared_and_thread_safe() {
    let pack = pack(compressed(FONT), FONT.len() as u64);
    assert!(!pack.is_decoded());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let pack = Arc::clone(&pack);
            std::thread::spawn(move || pack.decode().unwrap())
        })
        .collect();
    let decoded: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(decoded[0].as_ref(), FONT);
    assert!(decoded.iter().all(|bytes| Arc::ptr_eq(bytes, &decoded[0])));
    assert!(pack.is_decoded());
}

#[test]
fn corrupt_truncated_trailing_and_oversized_streams_fail() {
    let encoded = compressed(FONT);
    for bytes in [
        vec![],
        vec![0, 1, 2],
        encoded[..encoded.len() - 1].to_vec(),
        [encoded.as_slice(), b"trailing"].concat(),
    ] {
        let pack = pack(bytes, FONT.len() as u64);
        assert!(pack.decode().is_err());
        assert!(pack.decode().is_err());
    }
    assert!(pack(encoded.clone(), FONT.len() as u64 - 1)
        .decode()
        .is_err());
    assert!(FontPack::new(
        encoded,
        FontPackFormat::Zlib,
        FontPackLimits {
            max_encoded_bytes: 1,
            ..Default::default()
        }
    )
    .is_err());
    let bomb = pack(compressed(&vec![0; 1_000_000]), 1024);
    assert!(bomb.decode().is_err());
}

#[test]
fn raw_programs_share_input_and_coverage_is_only_a_hint() {
    let bytes: Arc<[u8]> = Arc::from(FONT);
    let pack = FontPack::new(
        Arc::clone(&bytes),
        FontPackFormat::Raw,
        FontPackLimits::default(),
    )
    .unwrap()
    .with_coverage(vec![(0x4E00, 0x4FFF), (0x20000, 0x20010)])
    .unwrap();
    assert!(pack.may_cover('中'));
    assert!(!pack.may_cover('A'));
    assert!(Arc::ptr_eq(&bytes, &pack.decode().unwrap()));
    assert!(
        FontPack::new(FONT, FontPackFormat::Raw, FontPackLimits::default())
            .unwrap()
            .with_coverage(vec![(9, 1)])
            .is_err()
    );
}

#[test]
fn provider_selects_and_admits_a_pack_once() {
    let pack = pack(compressed(FONT), FONT.len() as u64);
    let provider = FontProvider::memory_only();
    let id = provider
        .register_pack(Arc::clone(&pack), 0, &["ApplicationPack"])
        .unwrap();
    assert_eq!(
        provider
            .register_pack(Arc::clone(&pack), 0, &["ApplicationPack"])
            .unwrap(),
        id
    );
    assert_eq!(provider.len(), 0);
    assert_eq!(provider.bytes_used(), 0);
    assert_eq!(provider.reserved_pack_bytes(), FONT.len() as u64);
    assert!(!pack.is_decoded());
    assert!(provider
        .find_font("Unrelated", SubstituteHints::default(), None)
        .is_none());
    assert!(!pack.is_decoded());
    let found = provider
        .find_font("ApplicationPack", SubstituteHints::default(), None)
        .unwrap();
    assert_eq!(found.data.as_ref(), FONT);
    assert_eq!(provider.len(), 1);
    let generation = provider.generation();
    let again = provider.load_pack(id).unwrap();
    assert!(Arc::ptr_eq(&found.data, &provider.get(again).unwrap().data));
    assert_eq!(provider.generation(), generation);
}

#[test]
fn rebuilt_identical_packs_register_once() {
    // The wasm binding constructs a fresh `FontPack` from the host's bytes on
    // every call; repeats must not reserve the decoded budget again.
    let provider = FontProvider::new(
        false,
        FontProviderLimits {
            max_font_bytes: FONT.len() as u64,
            max_total_bytes: FONT.len() as u64 * 2,
            max_faces: 8,
        },
    );
    let encoded = compressed(FONT);
    let id = provider
        .register_pack(pack(encoded.clone(), FONT.len() as u64), 0, &["Pack"])
        .unwrap();
    for _ in 0..4 {
        assert_eq!(
            provider
                .register_pack(pack(encoded.clone(), FONT.len() as u64), 0, &["Pack"])
                .unwrap(),
            id
        );
    }
    assert_eq!(provider.reserved_pack_bytes(), FONT.len() as u64);
    // Another alias for the same program shares the reservation as well.
    provider
        .register_pack(pack(encoded, FONT.len() as u64), 0, &["Alias"])
        .unwrap();
    assert_eq!(provider.reserved_pack_bytes(), FONT.len() as u64);
    provider.register(FONT, 0).unwrap();
}

#[test]
fn failed_pack_does_not_hide_a_valid_candidate() {
    let provider = FontProvider::memory_only();
    provider
        .register_pack(pack(vec![0, 1, 2], 1024), 0, &["ApplicationPack"])
        .unwrap();
    provider
        .register_pack(
            pack(compressed(FONT), FONT.len() as u64),
            0,
            &["ApplicationPack"],
        )
        .unwrap();
    let found = provider
        .find_font("ApplicationPack", SubstituteHints::default(), None)
        .unwrap();
    assert_eq!(found.data.as_ref(), FONT);
    assert_eq!(provider.len(), 1);
    assert_eq!(provider.bytes_used(), FONT.len() as u64);
}

#[test]
fn provider_reserves_decoded_capacity_before_any_allocation() {
    let provider = FontProvider::new(
        false,
        FontProviderLimits {
            max_font_bytes: FONT.len() as u64,
            max_total_bytes: FONT.len() as u64,
            max_faces: 8,
        },
    );
    let shared = pack(compressed(FONT), FONT.len() as u64);
    let id = provider
        .register_pack(Arc::clone(&shared), 0, &["One"])
        .unwrap();
    provider
        .register_pack(Arc::clone(&shared), 0, &["Two"])
        .unwrap();
    assert_eq!(provider.reserved_pack_bytes(), FONT.len() as u64);
    // A different program needs its own reservation, which no longer fits.
    let other = compressed(&FONT[..FONT.len() - 1]);
    assert!(provider
        .register_pack(pack(other, FONT.len() as u64), 0, &["Three"])
        .is_err());
    assert!(provider.register(FONT, 0).is_err());
    assert!(!shared.is_decoded());
    assert!(provider.load_pack(id).is_ok());
    assert_eq!(provider.bytes_used(), FONT.len() as u64);
}

#[test]
fn concurrent_selection_admits_one_face_and_shares_one_program() {
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_pack(
            pack(compressed(FONT), FONT.len() as u64),
            0,
            &["SharedPack"],
        )
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let provider = Arc::clone(&provider);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                provider
                    .find_font("SharedPack", SubstituteHints::default(), None)
                    .unwrap()
                    .data
            })
        })
        .collect();
    let programs: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(programs
        .iter()
        .all(|bytes| Arc::ptr_eq(bytes, &programs[0])));
    assert_eq!(provider.len(), 1);
    assert_eq!(provider.bytes_used(), FONT.len() as u64);
    assert_eq!(provider.generation(), 2);
}

#[test]
fn decoded_invalid_programs_keep_their_reserved_budget() {
    let provider = FontProvider::new(
        false,
        FontProviderLimits {
            max_font_bytes: FONT.len() as u64,
            max_total_bytes: FONT.len() as u64 + 1024,
            max_faces: 8,
        },
    );
    let bad = pack(compressed(&[0; 1024]), 1024);
    let id = provider
        .register_pack(Arc::clone(&bad), 0, &["InvalidFace"])
        .unwrap();
    assert!(provider.load_pack(id).is_err());
    assert!(bad.is_decoded());
    assert_eq!(provider.bytes_used(), 0);
    assert_eq!(provider.reserved_pack_bytes(), 1024);
    assert!(provider
        .register_pack(
            pack(compressed(&[0; 1024]), FONT.len() as u64 + 1),
            0,
            &["TooLarge"]
        )
        .is_err());
    provider.register(FONT, 0).unwrap();
    assert!(provider
        .register_pack(pack(compressed(&[0; 1]), 1), 0, &["OverBudget"])
        .is_err());
}
