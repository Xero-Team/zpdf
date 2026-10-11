# Linux font measurements

All 57 common cases pass the unchanged +5% upper paired-bootstrap bound. This is an independent Linux comparison against production baseline 137ab2b with matching workloads and pinned third-party dependencies; it does not replace the separate macOS dataset.

Environment: Intel Xeon Gold 6133, x86_64 Linux container, Rust 1.96.0 / LLVM 22.1.2, glibc 2.35. Ten samples, 0.1 s warmup, 0.25 s measurement, default scheduling, alternating AB/BA. All earlier observations remain pooled.

Native validation reports 788 passes across 42 suites; private-input early returns do not establish corpus coverage. No hardware GPU coverage is claimed for this container.

[Recheck plan and scope](LINUX_RECHECK.md) · [All paired evidence](evidence/linux-recheck-final-pairs.json)

| Case | Pairs | Change | Paired 95% interval | Result |
| --- | ---: | ---: | --- | --- |
| `font_diagnostics/construct_owned` | 277 | +0.68% | -0.31% to +2.72% | pass |
| `font_diagnostics/copy_aligned` | 1109 | +3.30% | +2.89% to +3.68% | pass |
| `font_diagnostics/copy_heap` | 21 | -0.13% | -1.31% to +1.01% | pass |
| `font_diagnostics/copy_static` | 85 | +3.50% | +2.66% to +4.65% | pass |
| `font_diagnostics/copy_unaligned` | 21 | +2.63% | +1.59% to +3.46% | pass |
| `font_diagnostics/drop_loaded` | 149 | +0.74% | -0.29% to +1.59% | pass |
| `font_diagnostics/empty_widths` | 21 | +0.69% | -1.69% to +1.73% | pass |
| `font_diagnostics/parse_metrics` | 21 | -3.65% | -4.41% to -1.83% | pass |
| `font_diagnostics/type1_construct` | 21 | -0.24% | -0.66% to +0.65% | pass |
| `font_diagnostics/unicode_variable/configured_256` | 21 | -0.18% | -1.92% to +1.50% | pass |
| `font_diagnostics/unicode_variable/default_256` | 21 | -1.01% | -1.83% to +0.09% | pass |
| `fonts/load/cff_pdf` | 21 | -0.09% | -1.62% to +0.35% | pass |
| `fonts/load/document_cold_12_pages` | 21 | -9.22% | -14.56% to -2.01% | pass |
| `fonts/load/document_warm_12_pages` | 21 | -16.48% | -17.51% to -14.62% | pass |
| `fonts/load/truetype` | 21 | +1.19% | +0.03% to +2.02% | pass |
| `fonts/load/variable` | 21 | +2.91% | +2.75% to +3.87% | pass |
| `fonts/map/90ms-RKSJ-H` | 21 | -46.27% | -47.03% to -45.76% | pass |
| `fonts/map/B5pc-H` | 21 | -45.06% | -45.87% to -44.52% | pass |
| `fonts/map/EUC-H` | 21 | -40.94% | -42.07% to -40.65% | pass |
| `fonts/map/GBK-EUC-H` | 21 | -50.38% | -50.58% to -49.94% | pass |
| `fonts/map/GBpc-EUC-H` | 21 | -41.48% | -42.32% to -40.55% | pass |
| `fonts/map/KSC-EUC-H` | 21 | -50.65% | -50.97% to -49.57% | pass |
| `fonts/map/cjk_256` | 21 | -62.85% | -63.38% to -62.65% | pass |
| `fonts/map/latin_256` | 21 | -65.81% | -66.26% to -65.58% | pass |
| `fonts/map/missing_256` | 21 | -67.11% | -67.55% to -66.78% | pass |
| `fonts/map/supplementary_256` | 21 | -68.01% | -68.18% to -67.58% | pass |
| `fonts/outline/cff_256` | 21 | +2.48% | -2.05% to +4.34% | pass |
| `fonts/outline/repeated_256` | 21 | +0.17% | -0.92% to +2.22% | pass |
| `fonts/outline/unique_256` | 21 | +0.98% | +0.02% to +3.15% | pass |
| `fonts/outline/variable_256` | 21 | -0.52% | -1.34% to +1.79% | pass |
| `fonts/pipeline/extract/cjk_unique` | 21 | +3.37% | +3.13% to +4.45% | pass |
| `fonts/pipeline/extract/latin_repeated` | 21 | -60.58% | -61.56% to -59.97% | pass |
| `fonts/pipeline/extract/mixed_supplementary` | 277 | +4.02% | +3.70% to +4.53% | pass |
| `fonts/pipeline/render/cjk_unique@150` | 21 | -0.81% | -1.11% to +0.54% | pass |
| `fonts/pipeline/render/cjk_unique@300` | 21 | +0.69% | -0.39% to +1.19% | pass |
| `fonts/pipeline/render/cjk_unique@96` | 21 | -0.26% | -0.77% to +0.45% | pass |
| `fonts/pipeline/render/latin_repeated@150` | 21 | -4.41% | -7.26% to -1.16% | pass |
| `fonts/pipeline/render/latin_repeated@300` | 21 | +0.00% | -0.81% to +1.01% | pass |
| `fonts/pipeline/render/latin_repeated@96` | 21 | -0.22% | -3.18% to +2.45% | pass |
| `fonts/pipeline/render/mixed_supplementary@150` | 21 | -0.79% | -1.33% to +0.42% | pass |
| `fonts/pipeline/render/mixed_supplementary@300` | 21 | -0.09% | -0.59% to +1.00% | pass |
| `fonts/pipeline/render/mixed_supplementary@96` | 21 | +0.74% | -0.38% to +1.50% | pass |
| `fonts/pipeline/render_cff/150` | 21 | +0.89% | -0.03% to +2.11% | pass |
| `fonts/pipeline/render_cff/300` | 21 | -0.31% | -1.33% to +0.78% | pass |
| `fonts/pipeline/render_cff/96` | 21 | -0.31% | -2.24% to +1.58% | pass |
| `fonts/pipeline/total/cjk_unique@150` | 21 | +0.48% | -0.80% to +1.13% | pass |
| `fonts/pipeline/total/cjk_unique@300` | 21 | -0.34% | -0.73% to +0.39% | pass |
| `fonts/pipeline/total/cjk_unique@96` | 21 | -0.41% | -1.40% to +0.10% | pass |
| `fonts/pipeline/total/latin_repeated@150` | 21 | -5.98% | -6.87% to -4.76% | pass |
| `fonts/pipeline/total/latin_repeated@300` | 21 | -4.65% | -5.52% to -3.68% | pass |
| `fonts/pipeline/total/latin_repeated@96` | 21 | -6.63% | -7.03% to -5.66% | pass |
| `fonts/pipeline/total/mixed_supplementary@150` | 21 | -0.43% | -2.25% to +0.54% | pass |
| `fonts/pipeline/total/mixed_supplementary@300` | 21 | -0.03% | -1.60% to +0.58% | pass |
| `fonts/pipeline/total/mixed_supplementary@96` | 21 | -0.50% | -1.13% to +0.58% | pass |
| `fonts/pipeline/writer/cjk_unique` | 21 | +1.08% | -0.08% to +1.77% | pass |
| `fonts/pipeline/writer/latin_repeated` | 21 | -0.15% | -0.53% to +0.85% | pass |
| `fonts/pipeline/writer/mixed_supplementary` | 21 | +0.51% | -0.82% to +1.89% | pass |
