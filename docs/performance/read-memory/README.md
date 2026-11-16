# Read allocations and scan conversion

Tracks [#148](https://github.com/Xero-Team/zpdf/issues/148), against main
`8945e3a0152bf40c568bf33fca38d1b359163876` (the shared-stream implementation).

## Scope and behavior

PNG prediction reconstructs each row in its final output slice, keeping the
preceding output row as history. The internal tokenizer borrows valid UTF-8
operators; the public owned tokens, lossy text conversion, recovery and limits
retain their behavior. Read-only resource access shares cached object trees.
Cold shared resolution moves the parsed tree and compacts admitted strings and
arrays to retain no more slack than the old cached clone; uncached resolution
keeps its prior allocation behavior.

`DecodedStreamData` shares unchanged payloads and owns transformed output as a
Vec. The owned decode APIs remain available. No-op filters still consume their
original cumulative decode allowance. WASM opening removes the intermediate
Rust Vec before the persistent Arc; the JS/WASM input transfer and the final
owned PDF remain. See the [API/lifetime guide](../../guides/shared-stream-migration.md).

On the first minified draw of an eligible opaque Gray/RGB image XObject, one
converted source row feeds the existing box-filter intervals and rounding.
Column intervals are calculated once. Short images retain the full conversion
path when row/column scratch would cost more. JPEG still decodes its component
buffer in full; this is not streaming JPEG decompression. ICC, external masks,
color-key transparency, JPX, inline images and unminified draws retain their
established conversion paths. Repeated sizes retain the existing cache IDs,
admission budgets and two-decode/source-retention policy. No glyph/GPU cache or
clip/transparency-region allocation changes are included.

## Measurement protocol

Offline Linux aarch64 Docker, Rust 1.99.1, release profile, two CPUs, 5 GiB memory
limit, identical frozen dependency lock. The same benchmark source builds on
both revisions. A local trait adapts only the new shared-stream API to the old
Vec-returning API on main; all other calls are identical. The immutable input
fixture and warmed document cache stay outside the tracked allocation interval.
Cold resolution creates its parser inside the interval.

Each scenario runs in separate processes in 31 alternating AB/BA pairs, with
an untimed warmup and 11 samples per process. Fixed iteration counts, including
higher counts for near-zero-cost shared reads, are recorded in the measurement
plans before running. Time is the median paired process-median ratio, with
10,000 bootstrap resamples, seed 148, and a percentile 95% interval. All pairs
are retained; input and output hashes must agree. Verification rendering and
hash/metadata construction are outside the timed operation, except rendering
in the explicit end-to-end/render-only controls.

Requested heap excludes allocator metadata, realloc internals and the resident
input/cache. RSS measures the whole Linux process (including those fixtures,
code and allocator retention); it is not interchangeable with requested heap.
The WASM experiment records 64 KiB linear-memory pages immediately after the
first open, before verification rendering, and process maxRSS separately. Linear
memory does not shrink after freeing a document. Native timing results do not
establish timing on macOS, FreeBSD, Windows or browsers.

## Intermediate findings

The initial calibration found excess retained string/array capacity on cold
shared resolution. Compaction removed it. The first fixed comparison covered
17 scenarios; the second added a zero-cache control and compacted only admitted
objects. That second comparison showed a +4.67% end-to-end control result
(95% +3.51..+7.30%). Reusing its original binaries with longer samples reproduced
+3.08% (+1.57..+4.13%). These results are retained, not discarded.

The stage probe used unchanged production sources: interpretation improved
10.59%, rendering alone was near parity (+0.18%, -1.37..+0.88%), and the rebuilt
end-to-end probe changed sign. Inspection also found that the verification
helper appended an operator counter to the returned RGBA Vec *inside* the
end-to-end timing. This extra realloc doubled pixel capacity from 256 to 512
KiB. The final harness moves that verification work outside timing and heap
tracking and retains separate stage controls. This corrects the measurement;
it is not claimed as a renderer code optimization. Earlier aggregate timings
are not pooled with the corrected measurements.

## Final native results

`measure-3`, corrected harness; negative time changes are faster.

| Scenario | Peak requested bytes before → after | Retained bytes before → after | Time change | Paired 95% interval |
| --- | ---: | ---: | ---: | ---: |
| PNG Up, 4 MiB output | 12,593,673 → 12,587,905 | 4,194,304 → 4,194,304 | -5.81% | -6.50..-4.97% |
| PNG Paeth | 12,593,673 → 12,587,905 | 4,194,304 → 4,194,304 | -0.62% | -0.81..-0.54% |
| PNG partial last row | 12,593,673 → 12,587,905 | 4,194,304 → 4,194,304 | -6.14% | -6.70..-5.53% |
| Flate control | 8,465,570 → 8,464,739 | 8,388,608 → 8,388,608 | -1.02% | -3.03..+0.61% |
| Shared read, 8 MiB raw stream | 8,389,422 → 0 | 8,388,608 → 0 | -99.98% | -99.98..-99.98% |
| Shared read, 37-byte raw stream | 851 → 0 | 37 → 0 | -57.14% | -57.89..-56.36% |
| JPX pass-through, 8 MiB | 8,389,502 → 65 | 8,388,608 → 0 | -99.96% | -99.96..-99.96% |
| Legacy owned decode, 8 MiB | 8,389,422 → 8,388,608 | 8,388,608 → 8,388,608 | +0.15% | -0.40..+0.34% |
| Cold shared tree, 32 objects | 167,045 → 162,192 | 157,600 → 157,600 | -20.24% | -21.02..-18.09% |
| 32 cached shared objects | 256 → 256 | 256 → 256 | +2.77% | -3.56..+9.15% |
| 32 cached owned objects | 156,832 → 156,832 | 156,832 → 156,832 | +0.20% | -0.44..+0.42% |
| 12,288 operators | 35,529 → 35,528 | 0 → 0 | -20.95% | -21.79..-19.57% |
| Repeated Form/ExtGState | 206,185 → 188,490 | 180,224 → 180,224 | -45.38% | -45.77..-44.80% |
| Opaque RGB, 2048×1536 → 192×144 | 22,136,209 → 131,312 | 111,340 → 111,340 | -9.28% | -9.58..-9.00% |
| Opaque Gray, 2048×1536 → 192×144 | 15,844,755 → 130,753 | 111,340 → 111,340 | -22.04% | -22.18..-21.88% |
| RGB full-resolution conversion | 22,026,161 → 12,588,092 | 12,583,660 → 12,583,660 | -5.95% | -6.19..-5.60% |
| Interpret + CPU raster, 32 rectangles | 274,459 → 274,459 | 262,144 → 262,144 | -0.34% | -1.71..+0.09% |
| Shared tree, cache disabled | 194,328 → 194,328 | 191,904 → 191,904 | -20.04% | -20.42..-9.05% |
| Interpret only, 32 rectangles | 8,025 → 8,024 | 6,912 → 6,912 | -9.42% | -10.25..-8.42% |
| CPU raster only, 32 rectangles | 267,547 → 267,547 | 262,144 → 262,144 | +0.52% | -0.15..+1.63% |

No final time interval has a positive lower bound. The cached-read and renderer
controls remain near parity; their positive point estimates are not presented
as speedups. This does not exclude smaller differences or different workloads.
Requested peak/retained heap and allocation counts never increase in these
controls. The operator case falls from 12,319 to 29 allocations; PNG cases fall
from 1,055 to 22. The raw-stream result measures a warmed read API, not decoding
or whole-document throughput. Scan retained memory was already minified on main;
the improvement here is temporary conversion memory.

Whole-process high-water RSS medians (KiB), measured before output verification:

| Scenario | Before | After |
| --- | ---: | ---: |
| Shared read, 8 MiB raw stream | 27,424 | 26,752 |
| Opaque RGB, 2048×1536 → 192×144 | 53,072 | 31,484 |
| Opaque Gray, 2048×1536 → 192×144 | 28,496 | 13,048 |
| RGB full-resolution conversion | 62,072 | 43,648 |

RSS includes resident fixtures, code and allocator pages. Small RSS differences
in other controls fluctuate with process layout; all ranges are in `summary.json`.

## WASM opening results

The large fixture contains a 16 MiB unused stream; the small fixture has one
byte there. Opening/freeing is timed; rendering/edit-save verification is not.
Both packages produce identical pixels and use the same input bytes.

| Case | First-open pages before → after | Time change | Paired 95% interval | RSS peak median KiB before → after |
| --- | ---: | ---: | ---: | ---: |
| reader-large | 834 → 577 | -34.40% | -35.70..-33.40% | 152,216 → 135,872 |
| editor-large | 834 → 577 | -1.24% | -1.79..-0.87% | 158,036 → 141,736 |
| reader-small | 64 → 64 | -1.80% | -3.21..-1.26% | 76,636 → 76,672 |
| editor-small | 64 → 64 | -2.51% | -3.96..-0.95% | 76,696 → 76,740 |

Both large cases save 257 linear-memory pages (16.0625 MiB) at first open.
The editor performs additional existing initialization, so its timing benefit
is smaller. Small-case RSS differences of a few dozen KiB are within process
variation; their WASM page count is unchanged. These results do not eliminate
the persistent file copy or the JS-to-WASM transfer.

## Validation

- Targeted parser/content/image/writer/facade suites: 934 passed, one existing
  ignored system-font probe. Image tests were rerun after the explicit RGB accumulation
  cleanup: 83 passed.
- Formatting and Clippy with `-D warnings` passed for parser/content/image/writer,
  WASM and benchmark targets.
- Shipped WASM/JS and source fingerprint rebuilt together: 14 Node tests passed,
  including input reuse for reader/editor and password entry points. Font/CMap
  fixture reproducibility and 14 Python helper tests passed.
- Native/GPU/platform CI and Fuzz results are linked from the associated PR.

## Reproduction

Use the unchanged `crates/zpdf-benches/examples/read_memory.rs` on each revision
and the recorded dependency lock and iteration plans. In an isolated checkout,
copy `dependencies.lock` from this report to the workspace `Cargo.lock` first:

```sh
cargo build --offline --locked -p zpdf-benches --release --example read_memory
target/release/examples/read_memory scan-rgb 2
node --expose-gc crates/zpdf-wasm/www/open-memory.mjs /absolute/pkg reader large 8
```

The checked-in JSON evidence records hashes, all measured pairs, plans and
validation provenance. Synthetic inputs contain no third-party PDFs. Timing
confidence intervals describe these workloads and this machine; they are not a
universal no-regression guarantee.
