# Fuzz Harness Scaffolding Complete

**Date:** 2026-07-02  
**Status:** ✅ Complete (Windows MSVC type-checks; Linux/macOS ready to run)

## What was delivered

A complete **cargo-fuzz harness** for zpdf's parser attack surfaces:

1. **Five fuzz targets** covering the highest-value entry points:
   - `lexer` — token loop over `Lexer::next_token`
   - `object_parser` — indirect object parsing + stream extraction
   - `filters` — all codec paths (Flate/LZW/ASCIIHex/ASCII85/RunLength/CCITT/JBIG2/DCT/JPX) + predictor
   - `content_tokenizer` — content-stream operator scanner + inline image scan
   - `parse_pdf` — end-to-end whole-file parse + object resolution + stream decoding (deepest target)

2. **Seed corpora** under `fuzz/corpus/<target>/` with meaningful starting inputs:
   - Real small PDFs from `tests/corpus/`
   - Hand-written minimal indirect objects
   - Token-shaped snippets
   - Content-stream operators
   - Filter payloads with config headers

3. **Detached workspace** via `fuzz/Cargo.toml`'s own `[workspace]` table + `exclude = ["fuzz"]` in the root, so nightly-only fuzzing never contaminates normal builds.

4. **Comprehensive README** (`fuzz/README.md`) documenting:
   - Quick start (Linux/macOS)
   - Each target's invariant and coverage
   - Windows MSVC limitation (libFuzzer coverage symbols missing) + WSL workaround
   - CI integration snippet
   - Design rationale

## Verification

✅ **Type-checks** on Windows MSVC (both `cargo +nightly check` in `fuzz/` and `cargo build --workspace` in the root succeed)  
✅ **cargo-fuzz recognizes all 5 targets** (`cargo +nightly fuzz list`)  
✅ **Workspace isolation works** (normal `cargo build`/`test` never touch `fuzz/`)  
✅ **22 seed files** seeded across the five corpora

**Not tested:** actual fuzzing runs (requires Linux or macOS — Windows MSVC lacks libFuzzer's ELF section markers and ASAN runtime DLLs). The harness is **ready to run** on Linux CI or a developer's Linux/macOS workstation.

## Files created

```
fuzz/
├── Cargo.toml                         # Detached workspace, 5 [[bin]] targets
├── .gitignore                          # target/, corpus/, artifacts/
├── README.md                           # Full documentation
├── fuzz_targets/
│   ├── lexer.rs                        # Token loop
│   ├── object_parser.rs                # Indirect object + stream
│   ├── filters.rs                      # All codecs + predictor
│   ├── content_tokenizer.rs            # Operator scanner
│   └── parse_pdf.rs                    # Whole-file + resolve + decode
└── corpus/
    ├── lexer/ (3 seeds)
    ├── object_parser/ (4 seeds)
    ├── filters/ (4 seeds)
    ├── content_tokenizer/ (2 seeds)
    └── parse_pdf/ (9 seeds)
```

## Next steps (future work, not blocking)

1. **CI integration** — Add a GitHub Actions job on `ubuntu-latest` that runs each target for 60s as a smoke test. The README includes a snippet.

2. **Longer runs** — Schedule a nightly/weekly job that runs each target for hours. Store any crashes as CI artifacts.

3. **Triage crashes** — When a crash is found, reproduce it, fix the bug, and add a regression test to `crates/zpdf-parser/tests/`.

4. **Expand coverage** — Add new targets as new parser surfaces ship (e.g., a future `/Sig` signature validator).

## Why this matters

zpdf Phase 1.6 called for "cargo-fuzz targets: lexer, object parser" as a security/robustness milestone. This harness delivers that plus three additional high-value targets (filters, content tokenizer, whole-file parse). The parser crates are pure-safe-Rust (no unsafe blocks), so the primary value is catching **panics, hangs, and resource exhaustion** — exactly the contract the `tests/failed` corpus enforces, now with coverage-guided exploration over a much larger input space.

The fuzzer will find edge cases that hand-written tests miss: off-by-one errors, unbounded loops on cyclic structures, integer overflows in predictor calculations, and malformed token sequences that crash the lexer. Each finding hardens the parser against adversarial PDFs.

## Roadmap alignment

**Phase 1.6** (from `ROADMAP.md`):
- [x] cargo-fuzz targets: lexer, object parser  ← **This delivery**
- [ ] ParseLimits verification (recursion depth, stream size)  ← Partially covered (fuzz targets exercise the limits)
- [ ] Hand-written minimal PDF test cases  ← Seed corpora are minimal, but more traditional unit tests could complement

The harness is production-ready for Linux/macOS fuzzing and CI integration.
