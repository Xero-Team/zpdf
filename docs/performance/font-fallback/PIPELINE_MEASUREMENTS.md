# Original synthetic pipeline measurements

Independent Linux comparison against production baseline 137ab2b.
Each archived process runs the complete 135-case matrix. Every case has 21
AB/BA pairs, ten samples, 0.1 s warmup and 0.25 s requested measurement.
The paired bootstrap upper 95% bound must be at most +5% to pass.
Negative changes are faster. All observations are retained; these data
are separate from the per-case font and macOS protocols.

Result: **131 pass, 4 inconclusive, 0 regression**.

See [the protocol and audit](PIPELINE_RECHECK.md) and [all pairs](evidence/pipeline-pairs.json).

| Case | Change | 95% interval | Result |
| --- | ---: | ---: | --- |
| `stages/encode/clip-synthetic@150` | +0.83% | -1.97% .. +1.98% | pass |
| `stages/encode/clip-synthetic@300` | +0.93% | -0.60% .. +1.56% | pass |
| `stages/encode/clip-synthetic@96` | +0.18% | -0.18% .. +0.49% | pass |
| `stages/encode/curves-synthetic@150` | +0.18% | -0.67% .. +0.42% | pass |
| `stages/encode/curves-synthetic@300` | -0.47% | -1.09% .. +0.57% | pass |
| `stages/encode/curves-synthetic@96` | +0.04% | -2.10% .. +1.22% | pass |
| `stages/encode/image_rgb-synthetic@150` | -0.06% | -0.56% .. +0.67% | pass |
| `stages/encode/image_rgb-synthetic@300` | +0.20% | -0.73% .. +1.71% | pass |
| `stages/encode/image_rgb-synthetic@96` | +0.58% | -1.30% .. +2.23% | pass |
| `stages/encode/image_under_clip-synthetic@150` | +0.22% | -0.91% .. +1.69% | pass |
| `stages/encode/image_under_clip-synthetic@300` | +0.07% | -0.42% .. +0.76% | pass |
| `stages/encode/image_under_clip-synthetic@96` | +0.48% | -0.49% .. +0.97% | pass |
| `stages/encode/pattern_tiling-synthetic@150` | -0.81% | -1.31% .. +1.22% | pass |
| `stages/encode/pattern_tiling-synthetic@300` | -1.14% | -1.37% .. +0.28% | pass |
| `stages/encode/pattern_tiling-synthetic@96` | -0.22% | -1.36% .. +1.26% | pass |
| `stages/encode/pattern_tiling_uncolored-synthetic@150` | -0.32% | -1.61% .. +0.54% | pass |
| `stages/encode/pattern_tiling_uncolored-synthetic@300` | +1.53% | -0.25% .. +2.06% | pass |
| `stages/encode/pattern_tiling_uncolored-synthetic@96` | +0.21% | -0.47% .. +1.15% | pass |
| `stages/encode/rect_fills-synthetic@150` | +0.76% | -0.72% .. +1.90% | pass |
| `stages/encode/rect_fills-synthetic@300` | +0.82% | +0.22% .. +1.25% | pass |
| `stages/encode/rect_fills-synthetic@96` | +0.54% | -0.69% .. +2.32% | pass |
| `stages/encode/strokes-synthetic@150` | +0.11% | -0.44% .. +1.38% | pass |
| `stages/encode/strokes-synthetic@300` | +0.07% | -1.14% .. +1.38% | pass |
| `stages/encode/strokes-synthetic@96` | +0.93% | -0.80% .. +2.64% | pass |
| `stages/encode/text_type3-synthetic@150` | +0.67% | +0.07% .. +1.47% | pass |
| `stages/encode/text_type3-synthetic@300` | +0.22% | -0.49% .. +1.54% | pass |
| `stages/encode/text_type3-synthetic@96` | +1.32% | -0.88% .. +2.60% | pass |
| `stages/interpret/clip-synthetic@150` | -3.92% | -5.18% .. -3.20% | pass |
| `stages/interpret/clip-synthetic@300` | -4.07% | -5.93% .. -3.06% | pass |
| `stages/interpret/clip-synthetic@96` | -3.87% | -4.49% .. -2.39% | pass |
| `stages/interpret/curves-synthetic@150` | -7.56% | -9.21% .. -6.02% | pass |
| `stages/interpret/curves-synthetic@300` | -9.08% | -9.69% .. -7.31% | pass |
| `stages/interpret/curves-synthetic@96` | -8.14% | -9.97% .. -7.34% | pass |
| `stages/interpret/image_rgb-synthetic@150` | +0.22% | -1.13% .. +1.34% | pass |
| `stages/interpret/image_rgb-synthetic@300` | -1.67% | -2.77% .. -0.58% | pass |
| `stages/interpret/image_rgb-synthetic@96` | -0.45% | -1.72% .. +0.39% | pass |
| `stages/interpret/image_under_clip-synthetic@150` | -2.74% | -3.96% .. -2.10% | pass |
| `stages/interpret/image_under_clip-synthetic@300` | -2.26% | -2.93% .. -0.84% | pass |
| `stages/interpret/image_under_clip-synthetic@96` | -3.55% | -5.21% .. -2.42% | pass |
| `stages/interpret/pattern_tiling-synthetic@150` | -2.69% | -3.92% .. +0.05% | pass |
| `stages/interpret/pattern_tiling-synthetic@300` | -0.61% | -1.90% .. +1.53% | pass |
| `stages/interpret/pattern_tiling-synthetic@96` | -0.50% | -1.61% .. +1.23% | pass |
| `stages/interpret/pattern_tiling_uncolored-synthetic@150` | -2.22% | -3.23% .. +0.59% | pass |
| `stages/interpret/pattern_tiling_uncolored-synthetic@300` | -1.12% | -3.25% .. +1.00% | pass |
| `stages/interpret/pattern_tiling_uncolored-synthetic@96` | -1.67% | -2.44% .. -0.17% | pass |
| `stages/interpret/rect_fills-synthetic@150` | -8.26% | -9.10% .. -6.34% | pass |
| `stages/interpret/rect_fills-synthetic@300` | -7.91% | -9.25% .. -7.14% | pass |
| `stages/interpret/rect_fills-synthetic@96` | -8.91% | -9.62% .. -8.35% | pass |
| `stages/interpret/strokes-synthetic@150` | -6.39% | -8.02% .. -6.07% | pass |
| `stages/interpret/strokes-synthetic@300` | -6.57% | -8.05% .. -5.66% | pass |
| `stages/interpret/strokes-synthetic@96` | -7.73% | -8.89% .. -5.01% | pass |
| `stages/interpret/text_type3-synthetic@150` | -8.02% | -10.28% .. -6.89% | pass |
| `stages/interpret/text_type3-synthetic@300` | -9.38% | -11.29% .. -5.84% | pass |
| `stages/interpret/text_type3-synthetic@96` | -7.74% | -9.14% .. -6.62% | pass |
| `stages/parse/clip-synthetic@150` | +0.35% | -0.28% .. +0.84% | pass |
| `stages/parse/clip-synthetic@300` | +1.01% | +0.61% .. +2.08% | pass |
| `stages/parse/clip-synthetic@96` | +0.44% | -0.57% .. +2.47% | pass |
| `stages/parse/curves-synthetic@150` | +1.64% | +0.30% .. +2.60% | pass |
| `stages/parse/curves-synthetic@300` | +0.80% | -0.03% .. +2.23% | pass |
| `stages/parse/curves-synthetic@96` | +0.68% | -1.10% .. +2.11% | pass |
| `stages/parse/image_rgb-synthetic@150` | +0.14% | -1.05% .. +2.12% | pass |
| `stages/parse/image_rgb-synthetic@300` | +0.56% | -0.46% .. +1.19% | pass |
| `stages/parse/image_rgb-synthetic@96` | +0.56% | -0.62% .. +1.84% | pass |
| `stages/parse/image_under_clip-synthetic@150` | +0.68% | -0.36% .. +2.11% | pass |
| `stages/parse/image_under_clip-synthetic@300` | +0.37% | -0.18% .. +1.54% | pass |
| `stages/parse/image_under_clip-synthetic@96` | +0.54% | -0.07% .. +1.75% | pass |
| `stages/parse/pattern_tiling-synthetic@150` | +0.61% | -1.40% .. +1.47% | pass |
| `stages/parse/pattern_tiling-synthetic@300` | +1.36% | -0.07% .. +1.80% | pass |
| `stages/parse/pattern_tiling-synthetic@96` | +1.31% | -0.00% .. +2.95% | pass |
| `stages/parse/pattern_tiling_uncolored-synthetic@150` | +0.53% | -0.81% .. +1.54% | pass |
| `stages/parse/pattern_tiling_uncolored-synthetic@300` | +1.15% | +0.85% .. +2.20% | pass |
| `stages/parse/pattern_tiling_uncolored-synthetic@96` | +0.77% | -0.09% .. +1.78% | pass |
| `stages/parse/rect_fills-synthetic@150` | +1.39% | +0.41% .. +2.79% | pass |
| `stages/parse/rect_fills-synthetic@300` | +1.02% | +0.25% .. +1.55% | pass |
| `stages/parse/rect_fills-synthetic@96` | +1.15% | -0.48% .. +1.94% | pass |
| `stages/parse/strokes-synthetic@150` | +0.42% | -0.46% .. +1.40% | pass |
| `stages/parse/strokes-synthetic@300` | +0.32% | -0.22% .. +2.04% | pass |
| `stages/parse/strokes-synthetic@96` | +0.21% | -0.50% .. +1.91% | pass |
| `stages/parse/text_type3-synthetic@150` | +0.47% | -1.51% .. +1.67% | pass |
| `stages/parse/text_type3-synthetic@300` | +1.33% | +0.88% .. +1.73% | pass |
| `stages/parse/text_type3-synthetic@96` | +0.59% | +0.08% .. +1.54% | pass |
| `stages/render_cpu/clip-synthetic@150` | -0.66% | -3.10% .. +0.86% | pass |
| `stages/render_cpu/clip-synthetic@300` | -0.51% | -1.25% .. +1.39% | pass |
| `stages/render_cpu/clip-synthetic@96` | +1.14% | -0.71% .. +2.03% | pass |
| `stages/render_cpu/curves-synthetic@150` | +0.49% | -1.45% .. +1.16% | pass |
| `stages/render_cpu/curves-synthetic@300` | -0.24% | -15.48% .. +19.97% | inconclusive |
| `stages/render_cpu/curves-synthetic@96` | -0.45% | -1.72% .. +0.97% | pass |
| `stages/render_cpu/image_rgb-synthetic@150` | +0.05% | -1.15% .. +1.03% | pass |
| `stages/render_cpu/image_rgb-synthetic@300` | +0.19% | -0.37% .. +0.57% | pass |
| `stages/render_cpu/image_rgb-synthetic@96` | +0.87% | -1.10% .. +1.94% | pass |
| `stages/render_cpu/image_under_clip-synthetic@150` | -0.03% | -0.72% .. +1.19% | pass |
| `stages/render_cpu/image_under_clip-synthetic@300` | +0.94% | -0.53% .. +1.21% | pass |
| `stages/render_cpu/image_under_clip-synthetic@96` | +0.12% | -0.88% .. +1.34% | pass |
| `stages/render_cpu/pattern_tiling-synthetic@150` | -0.69% | -1.38% .. -0.16% | pass |
| `stages/render_cpu/pattern_tiling-synthetic@300` | -0.45% | -0.75% .. +0.32% | pass |
| `stages/render_cpu/pattern_tiling-synthetic@96` | -1.50% | -5.45% .. +0.47% | pass |
| `stages/render_cpu/pattern_tiling_uncolored-synthetic@150` | +0.52% | -0.94% .. +0.87% | pass |
| `stages/render_cpu/pattern_tiling_uncolored-synthetic@300` | -0.53% | -0.78% .. -0.10% | pass |
| `stages/render_cpu/pattern_tiling_uncolored-synthetic@96` | +0.55% | -0.07% .. +2.02% | pass |
| `stages/render_cpu/rect_fills-synthetic@150` | +0.78% | -0.54% .. +2.64% | pass |
| `stages/render_cpu/rect_fills-synthetic@300` | -16.79% | -23.48% .. +11.26% | inconclusive |
| `stages/render_cpu/rect_fills-synthetic@96` | -0.15% | -0.54% .. +0.40% | pass |
| `stages/render_cpu/strokes-synthetic@150` | +0.39% | -0.99% .. +1.72% | pass |
| `stages/render_cpu/strokes-synthetic@300` | -2.92% | -10.60% .. +14.12% | inconclusive |
| `stages/render_cpu/strokes-synthetic@96` | -0.43% | -2.32% .. +0.89% | pass |
| `stages/render_cpu/text_type3-synthetic@150` | +1.24% | -3.18% .. +5.62% | inconclusive |
| `stages/render_cpu/text_type3-synthetic@300` | -11.30% | -21.42% .. -0.72% | pass |
| `stages/render_cpu/text_type3-synthetic@96` | +0.55% | -1.53% .. +1.36% | pass |
| `stages/total/clip-synthetic@150` | -0.11% | -0.68% .. +0.80% | pass |
| `stages/total/clip-synthetic@300` | +0.40% | -0.58% .. +0.83% | pass |
| `stages/total/clip-synthetic@96` | -0.33% | -1.64% .. +0.59% | pass |
| `stages/total/curves-synthetic@150` | -0.17% | -1.95% .. +0.81% | pass |
| `stages/total/curves-synthetic@300` | +0.26% | -1.13% .. +0.81% | pass |
| `stages/total/curves-synthetic@96` | +1.37% | +0.56% .. +1.92% | pass |
| `stages/total/image_rgb-synthetic@150` | -0.85% | -1.41% .. +0.53% | pass |
| `stages/total/image_rgb-synthetic@300` | +0.20% | -1.00% .. +0.34% | pass |
| `stages/total/image_rgb-synthetic@96` | -0.25% | -1.23% .. +0.50% | pass |
| `stages/total/image_under_clip-synthetic@150` | -0.64% | -1.26% .. +0.49% | pass |
| `stages/total/image_under_clip-synthetic@300` | -0.05% | -0.26% .. +0.52% | pass |
| `stages/total/image_under_clip-synthetic@96` | -0.12% | -0.86% .. +1.13% | pass |
| `stages/total/pattern_tiling-synthetic@150` | -0.33% | -1.87% .. +0.58% | pass |
| `stages/total/pattern_tiling-synthetic@300` | +0.01% | -0.60% .. +0.80% | pass |
| `stages/total/pattern_tiling-synthetic@96` | +0.40% | -0.45% .. +0.90% | pass |
| `stages/total/pattern_tiling_uncolored-synthetic@150` | -0.53% | -2.07% .. +0.82% | pass |
| `stages/total/pattern_tiling_uncolored-synthetic@300` | +0.47% | -0.41% .. +1.06% | pass |
| `stages/total/pattern_tiling_uncolored-synthetic@96` | +0.25% | -1.17% .. +1.50% | pass |
| `stages/total/rect_fills-synthetic@150` | -0.48% | -0.93% .. +1.18% | pass |
| `stages/total/rect_fills-synthetic@300` | +0.13% | -0.24% .. +1.29% | pass |
| `stages/total/rect_fills-synthetic@96` | -1.30% | -2.39% .. +0.17% | pass |
| `stages/total/strokes-synthetic@150` | +0.79% | +0.31% .. +1.09% | pass |
| `stages/total/strokes-synthetic@300` | +0.59% | -0.25% .. +1.65% | pass |
| `stages/total/strokes-synthetic@96` | -0.26% | -1.25% .. +1.48% | pass |
| `stages/total/text_type3-synthetic@150` | +0.60% | +0.30% .. +0.85% | pass |
| `stages/total/text_type3-synthetic@300` | +1.12% | +0.54% .. +2.67% | pass |
| `stages/total/text_type3-synthetic@96` | -1.15% | -1.86% .. +1.28% | pass |
