# cargo-fuzz harness for zpdf

This directory contains a **libFuzzer-based fuzz harness** for zpdf's parser surfaces. Five targets cover the attack surfaces where malformed PDF input could trigger panics, hangs, or resource exhaustion.

## Quick start (Linux / macOS)

```bash
# Install nightly and cargo-fuzz (one-time):
rustup toolchain install nightly
cargo install cargo-fuzz

# List targets:
cargo +nightly fuzz list

# Build all targets (instrumented):
cargo +nightly fuzz build

# Run each target (e.g. 60 seconds per target):
cargo +nightly fuzz run lexer -- -max_total_time=60
cargo +nightly fuzz run object_parser -- -max_total_time=60
cargo +nightly fuzz run filters -- -max_total_time=60
cargo +nightly fuzz run content_tokenizer -- -max_total_time=60
cargo +nightly fuzz run parse_pdf -- -max_total_time=60

# Longer runs for CI or overnight:
cargo +nightly fuzz run parse_pdf -- -max_total_time=3600
```

Any crash or hang is saved to `artifacts/<target>/`. Reproducing a crash:
```bash
cargo +nightly fuzz run <target> artifacts/<target>/crash-<hash>
```

## Targets

### 1. `lexer`
Drives `Lexer::next_token` in a loop until EOF or error. Covers number/string/name/dict/array tokenization, the container-nesting depth guard, whitespace/comment skipping, and malformed token recovery.

**Invariant:** never panic, never hang (every input must terminate in a bounded number of tokens).

### 2. `object_parser`
Calls `ObjectParser::parse_indirect_at(0)` on arbitrary bytes. Exercises the `<num> <gen> obj … endobj` header, `N G R` ref promotion, and — the high-value part — **stream body extraction**: `/Length` trust vs. `endstream` scan, EOL stripping, and the `max_stream_bytes` guard.

**Invariant:** never panic, never hang, and respect the stream-size limit on large or malicious `/Length`.

### 3. `filters`
Feeds `zpdf_parser::filters::decode_stream` with a synthetic stream dictionary. The first byte of input selects a filter from the full codec list (Flate / LZW / ASCIIHex / ASCII85 / RunLength / CCITTFax / JBIG2 / DCT / JPX); the second byte configures predictor parameters (for Flate/LZW) or CCITT geometry; the remainder is the raw payload.

This structure lets the fuzzer explore **every codec** plus the PNG/TIFF predictor post-pass and its overflow guards (`checked_mul` for `colors × bpc × columns`).

**Invariant:** never panic, never hang, never over-allocate past the decompressor's internal caps.

### 4. `content_tokenizer`
Drives `ContentTokenizer::next_token` until `None`. Covers the operator/operand scanner that feeds the interpreter, including the awkward **inline image** (`BI … ID … EI`) length scan.

**Invariant:** never panic, never hang.

### 5. `parse_pdf` (the deepest target)
End-to-end whole-file parse: `PdfFile::parse_with_limits` on arbitrary bytes, then — if a file is produced — resolve every object and pull every stream's decoded data. This drives:

- Header detection (`%PDF-X.Y`)
- Xref-table / xref-stream / object-stream parsers
- The **tail-scan recovery path** (fallback when xref is corrupt)
- Lazy object decoding via `ObjectStore`
- The **full filter pipeline** including decryption

**Invariant:** never panic, never hang, never over-allocate — the same contract the `tests/failed` corpus enforces, but on fuzzer-generated inputs.

## Corpora

Seed inputs are committed under `seeds/<target>/` and copied into the working `corpus/<target>/` (git-ignored — the fuzzer grows it) by CI, or by hand on a fresh checkout:

```bash
for t in lexer object_parser filters content_tokenizer parse_pdf; do
  mkdir -p corpus/$t && cp -n seeds/$t/* corpus/$t/
done
```

These are hand-written minimal PDFs, token streams, and filter payloads that give the fuzzer meaningful starting points. Over time, the fuzzer expands coverage by mutating and recombining these seeds.

- `seeds/parse_pdf/` — real small PDFs from `tests/corpus/`
- `seeds/object_parser/` — minimal indirect objects (catalog, stream, ref body)
- `seeds/lexer/` — token-shaped snippets (dict, string, numbers)
- `seeds/content_tokenizer/` — content-stream operators + inline image
- `seeds/filters/` — config-header + payload for ASCIIHex / ASCII85 / RunLength / Flate-with-predictor

## Windows (MSVC) limitation

cargo-fuzz relies on **libFuzzer's coverage instrumentation**, which on Linux emits ELF section markers `__start___sancov_pcs` / `__stop___sancov_pcs`. On Windows/MSVC, those symbols don't exist (they're linker-generated on ELF), and the MSVC linker fails with unresolved externals. Additionally, AddressSanitizer runtime DLLs aren't shipped with rustup's Windows toolchains.

**Workaround for Windows developers:**
- Use **Windows Subsystem for Linux (WSL 2)** with a Linux nightly toolchain.
- Or defer fuzzing to CI (Linux runners work out of the box).

The targets **type-check and compile** on Windows in dev mode (`cargo +nightly check` succeeds), so the code itself is portable — only the instrumented fuzzing binary requires Linux or macOS.

## CI integration

`.github/workflows/fuzz.yml` runs all five targets on `ubuntu-latest`, one matrix job per target:

- **Nightly** (scheduled): 15 minutes per target.
- **Push to main** touching `fuzz/` or a parser-adjacent crate (`zpdf-core`, `zpdf-parser`, `zpdf-content`): 90-second smoke test.
- **Manual dispatch**: configurable seconds per target (default 300).

The working corpus is persisted between runs with `actions/cache` (keyed per target), so each night builds on the coverage the last one grew. A crash or hang fails the job and uploads `fuzz/artifacts/<target>/` as a `fuzz-artifacts-<target>` artifact; download it and reproduce locally with `cargo +nightly fuzz run <target> artifacts/<target>/<file>`.

## Design notes

### Why a detached workspace?

The fuzz harness is its own Cargo workspace (the `[workspace]` table in `fuzz/Cargo.toml` and `exclude = ["fuzz"]` in the root) so the nightly-only, sanitizer-instrumented build **never contaminates** normal `cargo build`/`test`/`clippy`. The root workspace sees `fuzz/` only as a directory to ignore.

### Why `#![no_main]` and `libfuzzer-sys`?

libFuzzer is a coverage-guided fuzzer baked into LLVM. `libfuzzer-sys` exposes the `fuzz_target!` macro that generates the C `LLVMFuzzerTestOneInput` entry point libFuzzer expects. Each target is a standalone binary (not a test or bench) that libFuzzer drives repeatedly with mutated inputs.

### Why these specific limits in `parse_pdf`?

```rust
let limits = ParseLimits {
    max_stream_bytes: 8 * 1024 * 1024,
    max_image_pixels: 4_000_000,
    max_page_operators: 100_000,
    max_objects: 100_000,
    ..Default::default()
};
```

Tight limits keep each fuzzer iteration **bounded** — recovery does an O(n) tail scan, and a malicious `/Length` or predictor could otherwise try to allocate multi-GB buffers. The fuzzer explores *logic* bugs (off-by-one, parsing errors, infinite loops) more than pathological-but-legal huge files.

### Zero unsafe code

zpdf's parser crates (`zpdf-parser`, `zpdf-core`, `zpdf-content`) contain **no unsafe blocks**. The value of AddressSanitizer (detecting use-after-free, buffer overruns) is thus lower than for C/C++ projects. The fuzzer's primary value here is catching:

- **Panics** (e.g. unwrap on malformed input, integer overflow)
- **Hangs** (e.g. unbounded loops, exponential predictor expansion)
- **Excessive allocation** (e.g. malicious `/Length` bypassing guards)

libFuzzer's coverage guidance is still valuable even without ASAN: it explores branches and finds code paths unit tests miss.

## Maintenance

- **Add new targets** when new parser surfaces ship (e.g. a future `/Sig` signature validator).
- **Triage crashes**: if a crash is found, reproduce it (`cargo +nightly fuzz run <target> artifacts/<target>/crash-<hash>`), fix the bug, and add a regression test to `crates/zpdf-parser/tests/`.
- **Refresh seeds** after major refactors: if the filter pipeline changes, update `seeds/filters/` with new config patterns.

## Further reading

- [cargo-fuzz book](https://rust-fuzz.github.io/book/cargo-fuzz.html)
- [libFuzzer documentation](https://llvm.org/docs/LibFuzzer.html)
- [Rust Fuzz project](https://github.com/rust-fuzz)
