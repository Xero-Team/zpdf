# Shared raw stream storage

Issue: [#144](https://github.com/Xero-Team/zpdf/issues/144). Baseline main:
`4c5e4a5995d072c99496368fd18ee7b27662c1d0` (after #143).
See the [API migration guide](../../guides/shared-stream-migration.md).

## Storage and allocation policy

`PdfStreamData` stores either an owned `Arc<[u8]>` directly or an Arc to a checked
file-range descriptor. Independent generated/decrypted buffers need **no extra
descriptor allocation**. On the measured 64-bit target its handle is 16 bytes;
`PdfObject` remains 48 bytes and `PdfStream` remains 40 bytes, matching main.
These sizes are implementation details, not an ABI promise.

File parsing shares payloads larger than 32 bytes. Shorter payloads copy into
owned storage to avoid allocating metadata larger than the saved bytes. Explicit
`from_shared` still makes checked views of every nonempty range without copying.
Cross-document graph copies detach payloads; same-document writes can share
views. Detaching a view releases unrelated file bytes, not necessarily its
allocation identity when the visible range already spans the whole allocation.

The parser transfers the newly parsed stream dictionary instead of cloning it,
compares the Catalog name without a temporary String, and converts a speculative
reference generation only when `R` follows. A 2,052-case differential test checks
reference values and cursor positions against the former numeric lookahead.
The small public stream constructor is inline so generated payloads do not pay
an out-of-line wrapper cost.

## Protocol

Measured 2026-11-15 in offline Linux aarch64 Docker, Rust 1.99.1, release profile,
two-CPU quota and 5 GiB memory limit. Each comparison uses the same synthetic
harness and resolved dependency lock for both binaries. Input and output hashes
match in every final pair, including imported output and rendered pixels.

Each case runs in separate processes in 31 alternating AB/BA pairs, with an
untimed warmup and 11 samples per process. Iterations per sample are fixed in
`measurement-plans.json`; short controls use thousands of iterations. Allocation
counters are disabled during timing. Time change is the median of paired
process-median ratios; intervals use 10,000 bootstrap resamples with seed 144
and the percentile 95% interval. All pairs are retained.

The final candidate (`measure-constructor`) and its independent confirmation
(`measure-confirm`) use identical archived executable hashes. Confirmation was
planned before inspecting the candidate and ran after the WASM rebuild, with no
concurrent target build. The two runs are reported separately, not pooled or
selected by their result. The table below is the confirmation run.

Heap counters measure requested live allocations **above the already resident
immutable input fixture**. They exclude allocator metadata, process RSS and
transient internal storage used by realloc. The 8 MiB PDF remains resident; this
change removes its duplicated stream payload, not the file. Imported source
copies are created inside the measured operation so unintended source retention
would appear in retained heap. The independent-stream control includes creating,
retaining and dropping the payloads, without a file parser.

## Final native results

| Case | Peak bytes before → after | Retained bytes before → after | Time change | Paired 95% interval |
| --- | --- | --- | --- | --- |
| Resolve one 8 MiB raw stream | 8,391,576 → 3,000 | 8,391,576 → 3,000 | -99.31% | -99.31..-99.30% |
| 1,024 streams, 128 bytes each | 2,042,696 → 1,944,392 | 2,042,696 → 1,944,392 | -6.02% | -6.97..-5.16% |
| 1,024 streams, one byte each | 1,919,816 → 1,919,816 | 1,919,816 → 1,919,816 | -7.03% | -8.04..-5.89% |
| 512 arrays of 32 integers | 1,687,372 → 1,687,372 | 1,687,372 → 1,687,372 | -18.57% | -19.53..-18.18% |
| Open without resolving the stream | 1,967 → 1,965 | 1,136 → 1,136 | -4.19% | -4.44..-3.83% |
| Clone 32 cached stream objects | 27,584 → 27,584 | 27,584 → 27,584 | -0.94% | -1.34..-0.33% |
| Resolve 8 MiB with object cache disabled | 8,391,436 → 2,046 | 8,390,622 → 2,046 | -99.35% | -99.36..-99.34% |
| Recover damaged xref, resolve 8 MiB | 8,391,464 → 2,888 | 8,391,464 → 2,888 | -13.68% | -14.89..-13.01% |
| Resolve encrypted 2 MiB stream | 6,294,772 → 4,197,652 | 2,101,330 → 2,101,330 | -2.05% | -3.15..-1.12% |
| Import four documents with unused 4 MiB payloads | 8,402,213 → 8,402,213 | 14,510 → 14,510 | -0.04% | -0.54..+0.49% |
| Open, interpret and rasterize 32 rectangles | 83,933 → 83,133 | 65,536 → 65,536 | -2.91% | -4.41..-0.75% |
| Construct 1,024 independent 128-byte streams | 196,736 → 196,736 | 196,608 → 196,608 | -26.91% | -27.52..-26.29% |
| Borrowed parsing of one 128-byte stream | 1,772 → 958 | 958 → 958 | -29.93% | -30.72..-28.67% |

Neither final run has a positive 95% interval lower bound in any case. Peak heap,
retained heap and allocation counts are never higher than baseline in these
controls. Independent streams use exactly the same 196,608 retained bytes and
2,049 allocations as main; the earlier net 40-byte payload cost is gone. Tiny
streams and scalar objects retain the same memory as main, rather than the
incidental savings of the rejected one-word representation.

There is **no universal no-regression guarantee**. The renderer measured +0.28%
(95% -1.40..+1.65%) in the candidate and -2.91% (-4.41..-0.75%) in confirmation;
no repeatable rendering speedup is claimed. Cached clones measured -0.86%
(-1.93..+0.41%) and -0.94% (-1.34..-0.33%). Imports remain near parity. Large raw
stream results concern parse/resolve and destruction, not decompression or full
PDF rendering. Native functional CI on other operating systems does not extend
these timing claims to those systems.

## Rejected and intermediate candidates

All eleven measurement phases remain in the evidence. The initial one-word
representation added a descriptor to independent payloads. Its longer run
showed a +0.73% cached-clone slowdown; after accessor/review changes, small/tiny
streams still slowed +0.45%/+0.48%. The user requested further optimization and
these costs were not accepted. One inline-only exploration briefly overlapped
formatting work; it remains explicitly excluded from final inference.

The owned/shared enum removed independent-payload allocation overhead but first
showed small-stream, cold-open and borrowed-parser regressions. Moving the stream
dictionary removed the small-stream regression; avoiding temporary strings
improved opening; deferring numeric reference conversion removed the scalar
regression. The generated-stream control then showed +0.58% (95% +0.29..+0.95%);
inlining its constructor removed that result. See `provenance.json` and the
fixed per-phase plans for each change. No earlier samples are pooled into the
final candidate or confirmation.

## Reproduction and validation

Copy `crates/zpdf-benches/examples/shared_streams.rs` unchanged into a checkout of
the baseline, and add `zpdf-parser.workspace = true` under the benchmark crate's
`[dev-dependencies]` for the borrowed-parser control. This is an existing internal
crate, not a new third-party dependency. Use the same dependency lock/toolchain
on both builds and archive each executable before building the other:

```sh
cargo build --release -p zpdf-benches --example shared_streams
target/release/examples/shared_streams raw-large 128
target/release/examples/shared_streams warm-read 4096
target/release/examples/shared_streams owned-streams 64
target/release/examples/shared_streams borrowed-parse 8192
```

Use every case/count in `constructor-plan.json` or `confirm-plan.json` inside
`measurement-plans.json`, alternating executable order per pair. Keep every JSON
result. `measurements.jsonl` contains all 7,894 process results; `summary.json`
contains confirmation statistics; `provenance.json` records snapshots, final
source files, image, dependency-lock and executable hashes. The earlier lock hash
is retained for phases before the benchmark's internal dev-dependency was added.

The raw-stream allocation regression fails on baseline with one large payload
allocation and passes here with zero. Isolated validation passed 514 native
Rust tests, Clippy, formatting and 17 ACL-runner tests. Coverage includes range
boundaries, short-stream ownership, parser equivalence, indirect lengths, cache
budgets, recovery, object streams, crypt filters, immutable ciphertext, encrypted
incremental replacement, visible-byte serialization and imported source release.
The parser fuzz target compares borrowed and shared storage. The rebuilt WASM
package passed 13 Node and 14 Python tests; its fingerprint binds the shipped
artifacts to the current Rust sources. Platform CI and Fuzz runs are linked in
the PR.
