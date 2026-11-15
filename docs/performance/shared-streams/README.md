# Shared raw stream storage

Issue: [#144](https://github.com/Xero-Team/zpdf/issues/144). Baseline main:
`4c5e4a5995d072c99496368fd18ee7b27662c1d0` (after #143).
See the [API migration guide](../../guides/shared-stream-migration.md).

Raw streams now hold checked ranges into the immutable file. The shared range
descriptor is out of line: its one-word handle avoids enlarging every
`PdfObject`, including scalar array elements and dictionary nodes. On the
measured 64-bit target, `PdfObject` shrinks from 48 to 40 bytes and `PdfStream`
from 40 to 32 bytes. These sizes are implementation details, not an ABI promise.
Decrypted and generated buffers have independent byte allocations. Cross-document
graph copies detach visible bytes, while same-document writes can share them.
An independently owned payload gains a 48-byte descriptor allocation on this
target; the handle saves 8 bytes. A standalone newly generated stream can thus
cost 40 additional requested bytes before accounting for dictionary/object
savings or descriptor sharing. This metadata tradeoff is not hidden by the
document-level measurements below.

## Protocol

Measured 2026-11-15 in offline Linux aarch64 Docker, Rust 1.99.1, release profile,
two-CPU quota and 5 GiB memory limit. The same synthetic harness and resolved
dependency lock are used on both revisions. Input and output hashes match in
every final pair, including imported writer output and rendered pixels.

Each case runs in separate processes in 31 alternating AB/BA pairs, with an
untimed warmup and 11 samples per process. Iterations per sample are fixed in
`measurement-plans.json`; the short control cases use thousands of iterations.
Allocation counters are disabled during timing. The reported time change is the
median of paired process-median ratios. Intervals use 10,000 bootstrap resamples
with seed 144 and the percentile 95% interval. All pairs are retained.

Heap counters measure requested live allocations **above the already resident
immutable input fixture**. They exclude allocator metadata, process RSS and
transient internal storage used by realloc. For example, the 8 MiB file itself
remains resident: the result below removes its duplicated stream payload, not
the file. Imported source copies are deliberately created inside the measured
operation, so accidentally retained source files would appear in retained heap.

## Final native results

| Case | Peak bytes before → after | Retained bytes before → after | Time change | Paired 95% interval |
| --- | --- | --- | --- | --- |
| Resolve one 8 MiB raw stream | 8,391,576 → 2,720 | 8,391,576 → 2,720 | -99.24% | -99.25..-99.24% |
| 1,024 streams, 128 bytes each | 2,042,696 → 1,747,696 | 2,042,696 → 1,747,696 | -0.30% | -0.90..+0.06% |
| 1,024 streams, one byte each | 1,919,816 → 1,747,696 | 1,919,816 → 1,747,696 | -0.32% | -0.70..+0.08% |
| 512 arrays of 32 integers | 1,687,372 → 1,416,948 | 1,687,372 → 1,416,948 | +0.39% | -0.30..+0.97% |
| Open without resolving the stream | 1,967 → 1,791 | 1,136 → 1,048 | -0.40% | -0.67..-0.14% |
| Clone 32 cached stream objects | 27,584 → 24,512 | 27,584 → 24,512 | -1.01% | -1.41..-0.57% |
| Resolve 8 MiB with object cache disabled | 8,391,436 → 2,588 | 8,390,622 → 1,862 | -99.30% | -99.30..-99.28% |
| Recover damaged xref, resolve 8 MiB | 8,391,464 → 2,608 | 8,391,464 → 2,608 | -12.70% | -13.47..-12.10% |
| Resolve encrypted 2 MiB stream | 6,294,772 → 4,197,340 | 2,101,330 → 2,100,970 | -1.82% | -2.04..-1.75% |
| Import four documents with unused 4 MiB payloads | 8,402,213 → 8,401,069 | 14,510 → 13,182 | -0.12% | -0.38..-0.05% |
| Open, interpret and rasterize 32 rectangles | 83,933 → 82,613 | 65,536 → 65,536 | +1.05% | -0.02..+1.80% |

Peak and retained heap do not increase in these cases. No final interval lies
wholly above zero, but this is **not a universal no-regression guarantee**.
The scalar and raster point estimates are slightly slower; the raster interval
in particular is close to zero and does not establish a speedup. The large raw
stream result measures parse/resolve and destruction, not decompression or full
PDF rendering. Decryption and imports add respectively one and eight descriptor
allocations, even though their peak/retained bytes fall. Allocation counts and
all timing samples are included in the raw evidence.

The first candidate showed a reproducible +0.73% cache-hit slowdown (31 pairs,
95% +0.29..+1.20%). It was not accepted as the final result. Exposing the cache-hit
path for inlining and reading the validated range length directly removes that
signal in the final candidate. Earlier measurements remain separate and are not
pooled with the final revision. An intermediate inline-only experiment briefly
overlapped formatting work; it is explicitly exploratory and excluded from the
final inference. The final measurement ran without concurrent target builds.

## Reproduction and validation

Copy `crates/zpdf-benches/examples/shared_streams.rs` unchanged into a checkout of
the baseline. Use the same lock resolution/toolchain for both builds and archive
each executable before building the other:

```sh
cargo build --release -p zpdf-benches --example shared_streams
target/release/examples/shared_streams raw-large 128
target/release/examples/shared_streams warm-read 4096
target/release/examples/shared_streams pipeline 48
```

Use every case/count in `measurement-plans.json`, alternate executable order
per pair, and keep every JSON result. `measurements.jsonl` records all four
measurement phases; `summary.json` contains the final phase, and
`provenance.json` records snapshot, image, dependency-lock and binary hashes.

The raw-stream allocation regression fails on baseline with one large payload
allocation and passes here with zero. Isolated tests cover checked and malformed
ranges, empty streams, borrowed/shared parser equivalence, indirect lengths,
cache budgets, recovery, object streams, crypt filters, immutable ciphertext,
encrypted incremental replacement, visible-byte serialization and imported
source release. The parser fuzz target compares the borrowed and shared APIs.
Platform CI and the shipped WASM runtime checks are recorded in the linked PR.
