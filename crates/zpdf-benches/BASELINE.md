# zpdf-benches baseline

Captured on the unmodified `main` (commit `df934aa`) to anchor the M1 CPU glyph
cache before/after. All numbers: release build, criterion, 150 DPI, CPU backend.
`Melem/s` = megapixels/sec throughput. Reproduce with:

```
cargo bench -p zpdf-benches --bench render
# then run the bench binary directly with --bench to actually measure:
./target/release/deps/render-<hash>.exe --bench cpu_render
./target/release/deps/render-<hash>.exe --bench full_pipeline
```

> NB: `cargo bench` runs criterion in *test* mode (verifies only). To measure,
> invoke the bench binary with the `--bench` flag (criterion 0.5 `cargo_bench_support`
> switches on it). criterion also caches results under `target/criterion/` for
> automatic regression reporting on the next run.

## cpu_render (render-only; parse + interpret excluded)

| corpus | page rect | raster | time (median) | throughput |
|---|---|---|---|---|
| text-testpdf-ai | — | ~2.2 Mpx | **205.5 ms** | 10.95 Melem/s |
| text-zzztest2 | — | ~1.4 Mpx | **26.8 ms** | 54.1 Melem/s |
| text-test8 (p0) | — | ~2.1 Mpx | 9.2 ms | 228 Melem/s |
| small-test6 | — | ~2.1 Mpx | 11.9 ms | 176 Melem/s |
| small-test3 | — | ~2.1 Mpx | 3.8 ms | 556 Melem/s |
| image-test10 (p0) | — | ~2.2 Mpx | 18.8 ms | 115 Melem/s |

## full_pipeline (open + interpret + render, CPU)

| corpus | time (median) |
|---|---|
| text-testpdf-ai | 274.6 ms |
| text-zzztest2 | 61.5 ms |
| text-test8 (p0) | 22.2 ms |
| small-test6 | 13.3 ms |
| small-test3 | 6.4 ms |
| image-test10 (p0) | 40.7 ms |

## read-out

- **Render is 50–75% of total wall-clock** on these pages (testpdf-ai: 205 ms of
  274 ms; zzztest2: 27 ms of 61 ms). → render optimization (M1) is the correct
  first target; interpreter dispatch (M5) is the next meaningful chunk.
- **text-testpdf-ai (205 ms) is the dominant text-heavy case** and the primary
  M1 validation target — the CPU glyph cache should move this the most.
- **text-test8 page 0 is sparse** (9 ms despite a 16.7 MB file — likely a cover);
  the corpus entry stays for diversity but is not a text-heavy target. zzztest2
  and testpdf-ai carry the text-heavy signal.
- image-test10 (19 ms render) carries no text → the glyph cache should show
  ~no change there (cache miss → fallback). That is the negative-control case
  for M1: a non-text page must not regress.
