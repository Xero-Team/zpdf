# Font benchmarks

> Corpus and pipeline targets (`stages`, `backend`, `batch`, `longform`) are covered in
> `crates/zpdf-benches/BASELINE.md`; render-performance design notes are in `PERFORMANCE.md`.
> This page covers how fonts perform: what is measured, how to compare, and the accepted results.

## Purpose and scope

The font targets measure the work that the corpus targets cannot isolate: font construction,
Unicode/CMap mapping, outlines, memory-font registration, per-character coverage selection,
lazy font packs, per-glyph fallback on a page, multilingual authoring and GPU glyph drawing.
Every input is a checked-in fixture (tiny generated TrueType fonts, a variable font, a Quartz
CFF subset, a six-code-point Noto Sans SC subset, Adobe-anchored CMap cases). They need **no
installed fonts, no network and no private PDFs**. These fixtures isolate coverage, placement
and per-glyph cost. They do not represent the size or typographic complexity of production CJK fonts.

## Targets and groups

Fixture builders live in `crates/zpdf-benches/src/font_{,fallback_,pack_,cmap_}workloads.rs`.
**Each workload checks its output before Criterion starts timing**: dropped glyphs, wrong
faces or shifted text panic in setup, so they cannot show up as a speedup.

| Group | Timed | Validated before timing |
|---|---|---|
| **`--bench fonts`** | | |
| `fonts/load` | TrueType and variable-font construction; CFF font loaded from a PDF; `load_page_fonts` over a 12-page document, cold (fresh document) and warm | Fixtures parse (text checks live in `fonts/pipeline`) |
| `fonts/map` | 256 `unicode_glyph` calls (repeated Latin, unique CJK, supplementary, missing); 256 decodes through six legacy CMaps (GBpc-EUC-H, GBK-EUC-H, B5pc-H, 90ms-RKSJ-H, EUC-H, KSC-EUC-H) | Each code hits or misses as expected; each CMap decodes its anchor code to the expected Unicode |
| `fonts/outline` | 256 outline extractions: repeated and unique TrueType glyphs, a real CFF glyph, a variable instance (`wght` 900) | The CFF glyph has the expected GID and an outline |
| `fonts/pipeline` | For three 256-glyph texts (`latin_repeated`, `cjk_unique`, `mixed_supplementary`): `extract` (warm document), `writer` (embed + subset + write one page), `render/<case>@{96,150,300}` (fresh CPU renderer, prebuilt display list), `total/<case>@dpi` (open in-memory PDF → fonts → interpret → render; no file I/O or PNG); `render_cff/{96,150,300}` (256 CFF glyphs) | Extracted text, every glyph ID, PDF advance and position, ink inside every glyph cell; CFF text and GIDs |
| `fonts/type1_variable` | Type 1 font construction; 256 Unicode lookups on a variable font at its default and a configured instance | Type 1 glyph ID and advance; variable lookup equals `ttf-parser`'s GID and advance for that instance |
| `font_provider/registration` | Register a face in a new memory-only provider; duplicate registration; name hit and miss | Duplicate returns the same face ID; hit found, miss absent |
| `font_provider/document` | Page setup (font loading + interpretation, 150 DPI) of a non-embedded font supplied by the provider, warm and cold; late registration after a miss; embedded-font page with an empty provider (ordinary-path control) | Full text checks as in `fonts/pipeline` |
| `font_coverage/resolve` | `resolve_character`: cold/warm hit and miss, 256 mixed requests, variation-selector lookup, late registration, first hit through a lazy zlib pack | Selected face and non-zero GID; misses return none; 224 of 256 mixed requests resolve; variation sequence picks its own glyph |
| `font_cmaps` | Seven Adobe-anchored cases (UCS-2, UTF-16, four-byte GBK2K, HKSCS, vertical JIS/GBK, Identity with a known collection): `lookup` (256 code→CID), `decode` (256 segment + Unicode), `pipeline/<case>/{embedded,substitute}` (parse → CPU render, 24 glyphs); `writer/{predefined_h,predefined_v,cff_nonidentity}` | Code→CID and code→Unicode match the pinned Adobe sources; pipelines check extraction, embedded vs substitute face, outline identity, W/W2 placement and ink; writer cases must build |
| **`--bench font_fallback`** | | |
| `font_fallback/pipeline` | 256 mixed codes (Latin, CJK, supplementary, emoji, one unrecoverable) in an embedded font that lacks most of them, recovered from registered fonts at 144 DPI: cold/warm document setup, warm page-cache interpretation, extraction, CPU render, SVG export; `vertical` (Identity-V), `text_clip` (`7 Tr`) and `form` (Form XObject) variants | Drawing face and GID of every glyph, PDF `/W` advances and positions, no outline for the unrecoverable code, ink in every visible cell, full extraction |
| `font_writer_fallback` | `add_text_with_fonts` + build: 64 scalars across three faces, 64 single-face scalars, rejection of unsupported text | Round-trip extraction equals the input |
| `font_packs/decode` | zlib decode cold and cached, raw cached, invalid stream cold and cached (cached failure), decoded-size-limit rejection | Decoded bytes equal the source; invalid and over-limit packs fail |
| `font_packs/provider` | Lazy pack registration; name selection cold (includes decode and admission) and warm; lookup that must skip an unrelated pack | The unrelated pack is never decoded (0 bytes used) |
| `font_packs/document` | Page whose CJK text is supplied only by a pack (Noto Sans SC subset): cold/warm setup, late registration, CPU render at 144 DPI | Extraction, substitute GIDs, real outlines, PDF widths and positions, ink per character |
| **`--bench font_gpu`** | | |
| `font_gpu/render_readback` | wgpu render + readback at 144 DPI: repeated Latin, unique CJK, mixed supplementary, vertical mixed, sheared, mirrored | CPU and GPU both draw ink; ≥ 70 % ink overlap; GPU ink within 75–125 % of CPU |

## Running and comparing

```bash
cargo bench -p zpdf-benches --bench fonts                            # load/map/outline/pipeline, provider, coverage, CMaps
cargo bench -p zpdf-benches --bench font_fallback                    # page + writer fallback, font packs
cargo bench -p zpdf-benches --bench font_gpu --features gpu-render   # opt-in; needs a real GPU adapter
cargo bench -p zpdf-benches --bench fonts --bench font_fallback -- --test   # CI smoke: validate, run each case once
cargo bench -p zpdf-benches --bench fonts -- font_coverage/          # one group (Criterion regex filter)
```

CI runs only the `--test` smoke. It checks fixtures and harness, **not timing**: on a shared
runner a 5 % wall-clock gate would fail at random (same reasoning as `BASELINE.md`).
`font_gpu` panics when no wgpu adapter can be created; it never skips silently. It prints
the adapter it used, so only compare runs made on the same adapter.

Use Criterion's named baselines for before/after comparisons. Run both sides from the same
checkout, so they share `target/criterion`, and on the same machine:

```bash
# on the reference commit
cargo bench -p zpdf-benches --bench fonts --bench font_fallback -- --save-baseline before
# on the candidate
cargo bench -p zpdf-benches --bench fonts --bench font_fallback -- --baseline before
```

- **Budgets.** `--warm-up-time`, `--measurement-time` and `--sample-size` override the
  targets' defaults. `-- --warm-up-time 0.1 --measurement-time 0.25 --sample-size 10` is the
  per-run budget used for acceptance. Pass the same values on both sides.
- **Noise first.** Use an idle machine with the same power settings for both runs. Before you
  trust a 5 % delta, compare the unchanged code against itself (`--save-baseline a`, then
  `--baseline a`). On a busy machine, identical builds have drifted past 5 %. If one
  comparison flags a case, rerun both sides before you call it a regression.
  `--noise-threshold 0.05` hides changes smaller than the gate.
- **Not the `baseline` binary.** The checked-in `baseline/baseline.json` holds only the
  corpus targets. `baseline --compare` lists font cases as "new" and does not judge them.

**Parallel interpretation.** One `FontProvider` is meant to be shared across threads.
`cargo run -p zpdf-benches --release --example font_parallel -- [seconds]` interprets the
same page on 1…N threads, once with a shared provider and once with a provider per thread,
and prints pages per second for both. Equal columns mean the shared provider adds no
contention; compare the ratio rather than absolute rates across machines.

## Results

**Acceptance rule.** Each case was measured as 21 alternating old/new pairs, each run with
10 samples, 0.1 s warm-up and 0.25 s measurement. A case passed only if the upper bound of
the paired-bootstrap 95 % interval of the median ratio was **≤ +5 %**. An interval that
straddled +5 % counted as inconclusive, which is not a pass. Ordinary-path controls were
compared with the code from before the fallback work. They ran on an **x86-64 Linux VM**,
the acceptance platform, and again on an **Apple Silicon Mac**. The new suites have no
pre-change version to compare against. For them, the table gives first-run medians on the
Mac and the accepted change of the final code against that first run. Negative means faster.

| Area | Measured | What it means |
|---|---|---|
| Ordinary embedded fonts: `fonts/*`, `fonts/type1_variable` (Linux) | **All 49 cases pass** (they also all pass on the Mac). Unicode→glyph lookup −63…−68 %, legacy CMap decode −41…−51 %, repeated-Latin extraction −61 %, 12-page font loading −16 % warm / −9 % cold, repeated-Latin end-to-end −5…−7 %. Every other case is within −4.4…+4.0 %; the worst upper bound is +4.5 % | Fallback support adds no cost to documents with embedded fonts, and the common lookups got faster |
| Existing synthetic pipeline: `stages`, 135 cases (Linux) | **All pass**; worst upper bound +4.1 %. Both sides rendered with `BatchSize::PerIteration` (one raster live per iteration) instead of the checked-in `SmallInput` | No regression outside the font path |
| Provider: `font_provider/*` | Registration and name hit/miss −1.5…−5.5 %; page setup with a provider (registered font, or embedded font with an empty provider) −65…−72 % against the first provider build | Pages with repeated codes reuse mapping decisions. Registration does no system-font scan or I/O |
| Coverage: `font_coverage/resolve` | First run: warm hit/miss ≈ 0.1 µs, variation lookup 0.12 µs, 256 mixed 31 µs, cold hit 3.1 µs, cold miss 2.9 µs, late registration 10.7 µs, first hit via lazy pack 76 µs. Final: cold miss −38 %, cold hit −8 %, the rest −0.3…−5 % | Decisions are cached per provider generation, so a repeat lookup costs about 0.1 µs. A cold miss no longer parses whole faces |
| Lazy packs: `font_packs/*` (2.5 KB fixture) | First run: lazy registration 1.0 µs, cold zlib decode 17 µs, cached decode 6 ns, first selection including decode 24 µs, warm selection 0.43 µs; CJK page setup 71 µs cold / 6.9 µs warm, 156 µs render. Final: −4.7…+0.5 % | Registering a pack decodes nothing. Each pack is decoded once and a failed decode is cached. Unrelated packs are never touched |
| Page fallback: `font_fallback/pipeline` (256 mixed codes) | First run: setup 0.40 ms cold / 0.30 ms warm, warm page-cache interpretation 0.27 ms, CPU render 3.8 ms at 144 DPI; vertical (8 rows) 0.10 ms; clip mode (16 rows) 0.51 ms. Final: −2.2…+2.2 % | Setup, fallback resolution included, takes about a tenth of the CPU render time |
| Writer fallback: `font_writer_fallback` | First run: 64 scalars across three faces 1.95 ms vs one face 0.99 ms; rejecting unsupported text 2.0 µs. Final: +2.5 %, +2.0 %, −50 % | Each extra face adds its own embedding and subsetting. Unsupported text is rejected before any metrics or layout work |
| CMaps: `font_cmaps` | First run: 256 code→CID lookups 19–31 µs (Identity 2.6 µs); 24-glyph parse→render 0.39–0.45 ms, except Identity-with-collection substitution at 2.07 ms. Final: lookup −37…−51 % for UCS-2/GBK2K/HKSCS/vertical maps (UTF-16 −11 %, Identity −5 %); decode −16…−27 % for those maps; Identity substitution −75 %; other pipelines −10…+1 %; writer +0.5…+1.8 % | A CID lookup costs about 0.1 µs per code or less. Known collections now resolve canonical CIDs on demand instead of scanning them at font load |
| Shared provider, parallel: `font_parallel` (Linux, 40 threads) | Embedded-CJK page with one registered face: 20.4× at 40 threads, 0.99 of one provider per thread; before read-mostly, sharded glyph checks it fell to 0.04 (below the single-thread rate) from 16 threads on. Mixed fallback: 0.83 → 0.99. Apple Silicon (10 cores): 0.19 → 1.00. Single-thread rates unchanged | Threads rendering with one provider no longer serialize on glyph checks |
| GPU: `font_gpu/render_readback` (Mac, Metal) | Repeated Latin −16 %, mixed supplementary −12 %, the other four −5.9…+2.7 % | Checking the glyph atlas before extracting outlines pays off for repeated glyphs |

**Caveats.** On macOS, allocation-dominated microbenchmarks are unreliable at the 5 % level.
A plain 21 KB buffer copy, with no font code involved, moved by about 17 % between two builds
because of allocator placement, while staying flat on Linux. That is why Linux is the
acceptance platform. CPU render cases return a full page raster, so they depend on Criterion
batching (`SmallInput` keeps several pages alive); compare only runs with identical batching.
GPU figures are render + readback wall time, since Metal exposes no timestamp queries. None of
these numbers covers full-size CJK font download, decode or memory, or the private PDF corpus.
