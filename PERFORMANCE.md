# zpdf 性能优化设计文档

> 本文档是 zpdf 性能工作的设计与执行纲领。决策记录来自一轮系统性 grill，覆盖
> 目标、度量、保真契约与优化选择。后续每项优化都应先回到本文档确认范围与契约，
> 再动手。

---

## 1. 目标与非目标

### 目标
- **单页渲染延迟**：降低 CPU 与 GPU 两条后端的端到端单页渲染墙钟时间。
- **批量 / 多页吞吐**：降低连续渲染多页（CLI `render`/`convert`/`split`/`optimize`）的
  总耗时，使跨页资源复用成为一等公民，而非次要收益。
- **可度量**：建立可持续运行的基准测试套件，让每一项优化都能被证明“更快且不回退”。

### 非目标
- 不追求峰值内存最小化（现有预算上限已兜底；本阶段不收紧 `ParseLimits`）。
- 不在本阶段引入会改变保真契约的架构级 GPU 路径（原生 gradient/mesh shader、GPU 字形
  栅格化），除非单独决策放宽契约（见 §2.4）。

---

## 2. 决策记录（grilling 结论）

| # | 决策 | 结论 |
|---|------|------|
| 2.1 | 优化主目标 | **同时**覆盖单页延迟（两后端）与批量吞吐 |
| 2.2 | 基准框架 | **criterion**（仅 dev-dep）；GPU 基准复用共享 `GpuContext`，包 `last_gpu_time_ns()` |
| 2.3 | 基准结构 | **per-stage 微基准 + 端到端 corpus 基准** 双轨 |
| 2.4 | 保真契约 | 默认 **CPU oracle 冻结、GPU parity 维持**；对字形缓存做**有界放宽**（亚像素定位） |
| 2.5 | 首项优化 | **CPU 字形缓存**（亚像素量化光栅缓存） |
| 2.6 | 字形缓存作用域 | **先 per-page**（`CpuRenderer` 内，`begin_page` 清空），度量后若批量收益明显再 lift 到跨页 |
| 2.7 | 字形缓存保真 | **亚像素量化光栅缓存**：键含 subpixel bin，8×8=64 bin，round-to-nearest，漂移 ≤ 1/16 px |
| 2.8 | 字形缓存驱逐 | **无驱逐 + cap 回退**（镜像 GPU atlas）；cap 64 MiB/页，可配；超 cap 新字形走当前 on-the-fly 路径 |
| 2.9 | 验证严格度 | **完整**：速度 + 漂移量化（`zpdf compare` CPU-before vs CPU-after）+ GPU 重基线（GPU vs new-CPU） |

---

## 3. 现状与热点分析

阅读全流程（parser → content → display-list → CPU/GPU 后端）后确认的热点：

### 3.1 Parser（`zpdf-parser`）
- `Lexer` 逐字节 token 化；每个 token 一次 `String`/`from_utf8_lossy` 分配。
- `maybe_resolve_ref` 对**每个** Integer 做投机 lookahead + 回溯。
- `XrefTable` 为 `HashMap`；`find_startxref` 全缓冲 `rposition` 扫描（每文件一次）。
- **结论**：parser 一次性开销，相对渲染很轻。暂不优先处理。

### 3.2 Content（`zpdf-content`）
- `ContentTokenizer` 每算符一次 `String` 分配；`interpreter.rs:2708` 用 `op.as_str()` 分发。
- 操作数栈 `Vec<PdfObject>`，gstate 栈 `Vec<GraphicsState>`。
- `PdfFunction::eval` 每次 `Vec<f64>` 分配（`clamped`/`coord`/`out`）——但**非逐像素**：
  shading 在 build 时预采样 256 项 LUT。
- **结论**：算符分发与函数栈分配是单页中频热点，列为短名单第 4 项。

### 3.3 Shading（`zpdf-content/shading.rs`）
- axial/radial **两后端都在 CPU 栅格化**：逐像素 `param_at`（radial = 二次方程 + sqrt）+ LUT
  查找；mesh = Gouraud 扫描线填充。结果作为 image 上传。**GPU 不跑 gradient shader。**
- **结论**：受 §2.4 严格保真约束，原生 shader 路径暂不入选。

### 3.4 CPU 渲染（`zpdf-render-cpu`）—— **首项目标**
- **无字形缓存**：`render_outline_glyphs` 对**每个字形实例**重新 `outline_to_pixel` 构建
  `tiny_skia::Path` 并 `fill_path` 栅格化。文字密集页为最大单页 CPU 开销。
- Clip = 每个 clip 层级一个**全栅格** `tiny_skia::Mask`（bbox-scoped 相交，但全页分配/清零）。
- Soft-mask 子渲染器开整页 `Pixmap`；每命令 `Instant::now()` 截止检查。
- **结论**：字形缓存 = 首项；clip bbox-scope = 短名单第 3 项。

### 3.5 GPU 渲染（`zpdf-render-wgpu`）
- 已有：per-page 2048² 字形 atlas（tiny-skia 栅格、millipixel key、无驱逐）、run-length
  draw-call batching、`LayerPool` 回收、opt-in GPU timing（`last_gpu_time_ns`）。
- **每页重建** buffers/textures/bind-groups（无 pool/ring buffer）；images 每页上传（无跨页纹理缓存）。
- **结论**：GPU 资源池 = 短名单第 1 项（最大批量收益）；跨页 atlas = 第 2 项。

### 3.6 基准设施现状
- **零**：无 criterion/divan/iai，无 `[[bench]]`。`wgpu/benches/` 是 vendored 依赖，不算。
- 语料：`tests/corpus/` 9 个合成 PDF；`tests/failed/` 618 个对抗 PDF；命名目录下另有真实 PDF
  （test3 113KB / test6 96KB / test9 1.1MB / test8 16.7MB / zzztest/2 14.7MB / testpdf 5.9MB /
  test10 35MB / test5 150MB，中英混合扫描+文本）。
- GPU timing 以 test 形式存在（`tests/timing.rs`）；`zpdf compare` 为 CPU↔GPU 保真 oracle。

---

## 4. 基准测试基础设施

### 4.1 框架：criterion
- 作为 dev-dependency 引入，仅参与 `cargo bench`，不影响 `cargo build`/`test`/`clippy`。
- 用 `BenchmarkGroup<Throughput>` 做 corpus×DPI×backend 参数化基准；用 `iter_custom`/
  `iter_with_setup` 隔离 setup（解析、interpret）与被测阶段（render）。

### 4.2 新 crate：`zpdf-benches`
- 独立 crate，依赖 `zpdf` facade + criterion；`gpu-render` feature gate GPU 基准。
- 放入 workspace `members`。
- 理由：把基准隔离在库 crate 之外，避免 dev-dep 污染库的 `Cargo.toml`。

### 4.3 per-stage 微基准（合成输入）
| 阶段 | 输入 | 测什么 |
|------|------|--------|
| lexer | 合成对象字节数组 | `Lexer::next_token` 吞吐 |
| xref | 合成 xref+stream | `parse_xref_and_trailer` |
| content-interpret | 合成 content stream | `ContentInterpreter::interpret` → DL |
| dl-build | （含在 interpret 内） | `Vec<RenderCommand>` 构建成本 |
| cpu-render | 合成 DisplayList（text/vec/image 各一） | `CpuRenderer::render_display_list` |
| gpu-render | 同上 | `WgpuRenderer::render_display_list`（GPU pass 用 `last_gpu_time_ns`） |

### 4.4 端到端基准（真实语料）
- 从命名目录精选代表性页：text-heavy（testpdf、zzztest、test8）、vector-heavy（corpus/strokes、
  curves）、image-heavy（test10/test5 单页）、混合。
- 矩阵：语料 × {96, 150} DPI × {cpu, wgpu}。
- 吞吐维度：同一文档连续渲染 N 页，度量均摊后每页耗时（验证跨页复用收益）。

### 4.5 GPU 基准特殊处理
- 复用一个 `GpuContext` 贯穿整个 group，**设备初始化与 adapter 选择不计入**被测时间
  （setup 阶段）。
- 用 `with_gpu_timing(true)`；每次迭代记录 `wall`（含 readback）与 `gpu_pass`（`last_gpu_time_ns`）。
- 注意：timestamp readback 引入额外同步点，GPU pass 时间与 wall 的差即 host→device 同步开销——
  这正是资源池优化的度量标的。

### 4.6 语料库
- 不用 `tests/failed/`（对抗、非真实负载）；不用 `tests/corpus/`（太合成）。
- 精选真实 PDF 的**单代表页**入端到端矩阵；多页吞吐用文档前 N 页。

---

## 5. 优化 #1：CPU 字形缓存

### 5.1 动机
`render_outline_glyphs`（`render-cpu/src/lib.rs:1286`）对每个字形实例执行完整
`outline_to_pixel` → `build_outline_transformed_path` → `fill_path`。文字密集页这是 CPU
单页最大开销。GPU atlas 已证明：同一 (font, glyph, device-px size) 的栅格可复用，只平移
blit。将同一模式搬到 CPU，并用亚像素量化解决“分数笔位 → 不同 AA 边”问题。

### 5.2 设计：亚像素量化光栅缓存
- 用 `tiny_skia::Mask`（R8 AA 覆盖）栅格化字形轮廓一次，按 key 缓存；命中时用
  `coverage × color×alpha → premul RGBA` 合成进一块可复用 scratch pixmap，再
  `pixmap.draw_pixmap` 平移 blit（接受 `current_clip`）。
- **亚像素量化**：把笔位的小数部分量化进 8 个 bin（x、y 各 8，共 64）。栅格化时用
  代表性亚像素偏移（bin 中心），blit 时对齐到 bin，漂移 = |real_frac − bin_center| ≤ 1/16 px。
- 这是标准文本渲染器技术（FreeType 亚像素定位），漂移有界、视觉可忽略，且可论证地
  优于“精确浮点 AA 在变 zoom 下的抖动”。

### 5.3 缓存键
```
struct GlyphKey {
    font_id: u32,
    glyph_id: u16,
    x_millipx_per_em: i32,   // 镜像 GPU atlas 的 millipixel 粒度（§3.5 / glyph_atlas.rs）
    y_millipx_per_em: i32,
    sx_bin: u8,              // 0..8，笔位 x 小数 round-to-nearest 到 1/8
    sy_bin: u8,             // 0..8
}
```
- `x/y_millipx_per_em`：复用 `axis_aligned_px_per_em`（`glyph.rs:133`）的公式，保证与 GPU
  atlas 同粒度（millipixel 而非整像素，避免 body-text 的相对畸变，见 `GlyphKey` doc 注释）。
- `sx_bin/sy_bin`：由设备像素笔位 `origin = outline_to_pixel(0,0,glyph.x,tm,&x)` 的小数部分
  `((frac * 8.0).round() as i32).rem_euclid(8) as u8` 得到。
- 代表性亚像素偏移：`srx = (sx_bin as f32 + 0.5) / 8.0`（bin 中心）。

### 5.4 轴对齐限制与回退
- 复用 `is_axis_aligned`（`glyph.rs:116`）：`b≈0 && c≈0 && a>0 && d>0 && h_scale>0`。
- 旋转/剪切/镜像的 run **不走缓存**，落回现有 `outline_to_pixel`+`fill_path`（bit-identical，
  保真契约不受影响）。
- 单个字形若退化解体轮廓 / 超 cap / atlas 不容纳，`get_or_rasterize → None`，落回当前路径。
- 加 `ZPDF_CPU_GLYPH_CACHE=0` 调试开关（镜像 GPU 的 `ZPDF_GPU_GLYPH_ATLAS`），用于 diff 时
  隔离缓存带来的 AA delta 与既有基线。

### 5.5 复合路径（tint + draw_pixmap）
- 缓存的是**无色覆盖 mask**；颜色在 blit 时合成：
  - 复用一块 scratch `Pixmap`（字形 bbox 大小，复用避免每字分配），对 mask 的每个覆盖像素写
    `premul = (color×alpha) × coverage`。
  - `pixmap.draw_pixmap(0,0, scratch_ref, &PixmapPaint{quality:Nearest}, Transform::translate(ox,oy), current_clip.as_ref())`。
- `draw_pixmap` 接受 `Option<&Mask>` clip（与 `render_image` 一致），故当前 clip 正确生效。
- 落地位 `ox = floor(origin.x) + srx - entry.pen_x`，`oy` 同理（`pen_x/pen_y` = 字形 font-space
  原点在栅格内的偏移，复用 `glyph_atlas.rs` 的 `AtlasEntry` 语义）。

### 5.6 生命周期与驱逐
- **作用域（§2.6）**：`GlyphCache` 字段挂在 `CpuRenderer`，`begin_page` 清空，`end_page` 释放。
- **驱逐（§2.8）**：无 LRU。累计字节超 `max_cpu_glyph_cache_bytes` 时，新字形 `get_or_rasterize`
  返回 `None` → 走 on-the-fly 栅格化（正确，仅未缓存）。镜像 GPU atlas 的“无驱逐 + 回退”哲学。
- 已记录的 quad/scratch 仍引用旧槽位，故绝不覆写已分配槽（同 GPU atlas 约束）。
- LRU 推迟到“度量显示工作集逼近 cap”再评估。

### 5.7 ParseLimits 字段
`zpdf-core/src/limits.rs` 新增：
```
/// Maximum total bytes for the CPU per-page glyph coverage cache.
/// Default: 64 MiB.
pub max_cpu_glyph_cache_bytes: u64,
```
- `CpuRenderer::with_limits` 读取该值；`GlyphCache` 据此判定 cap 回退。
- 与既有 `max_*_cache_bytes` 字段并列，风格一致。

### 5.8 Type3 不缓存
- Type3 字形是 content stream（`render_type3_glyphs`），栅格不可复用且语义不同。维持现状，
  不进缓存路径（镜像 GPU `glyph.rs` 的 `render_type3` 分支）。

### 5.9 落地文件清单
| 文件 | 改动 |
|------|------|
| `zpdf-core/src/limits.rs` | 加 `max_cpu_glyph_cache_bytes` 字段 + 默认值 |
| `zpdf-render-cpu/src/glyph_cache.rs` | **新建**：`GlyphCache`、`GlyphKey`、`get_or_rasterize`、scratch 合成 |
| `zpdf-render-cpu/src/lib.rs` | `CpuRenderer` 加 `glyph_cache` 字段；`render_outline_glyphs` 分流缓存/回退；`begin_page` 清空、`with_limits` 读 cap |
| `zpdf-benches/` | **新建 crate**：criterion 基准（§4） |
| `zpdf/Cargo.toml`（facade）| 若 `GlyphCache`/类型需 re-export 则补；否则不动 |

---

## 6. 验证协议

### 6.1 速度
- 微基准：合成 text DisplayList，N 个重复字形，before/after criterion 对比。
- 端到端：text-heavy 真实 PDF（testpdf、zzztest、test8）CPU render 墙钟 before/after。
- 门槛：text-heavy 页 after 明显更快；非 text 页（vector/image）不回退（缓存不命中即回退）。

### 6.2 漂移量化（证明“仅亚像素定位”）
- `cargo run -p zpdf-cli -- render <pdf> -p <i> -o before.png`（缓存关 / 旧代码）
- 启用缓存后 `... -o after.png`
- `cargo run -p zpdf-cli -- compare before.png after.png`
- 期望：MAE/RMSE 与 max-channel-diff 反映的正是 ≤1/16 px 的亚像素位移——**小且均匀**，
  无结构性/覆盖差异（无字形缺失/错位/大小变）。
- 逐像素可视化差异（`compare` 的红点叠加）应只出现在字形边缘 AA 像素。

### 6.3 GPU 重基线
- 新 CPU oracle（启用亚像素缓存）渲染 → `new_cpu.png`
- `WgpuRenderer` 渲染同页 → `gpu.png`
- `compare new_cpu.png gpu.png`：确立新的 parity 基线（GPU 自身仍用其 atlas，亚像素行为
  独立；两者都做亚像素定位，期望 parity 不劣化、甚至因双方都量化而更稳）。
- 在 corpus 多页上确认新基线稳定。

---

## 7. 后续优化优先级（短名单）

按“对两目标的预期影响 × 置信度”排序：

### 7.1 GPU 资源池（最大批量收益）
- `end_page` 每页 `create_buffer_init`/`create_texture`/`create_bind_group` → 跨页复用
  ring/pool buffer + texture + bind-group 缓存。
- 度量：多页吞吐 after 显著提升；GPU pass/wall 差（同步开销）下降。
- 保真：bit-identical（只改资源生命周期，不改绘制）。

### 7.2 跨页 GPU 字形 atlas
- atlas 当前 `begin_page` 重建 → 跨页保留 + 满 cap 回退。
- 前置：先度量单页 atlas 命中率与重建成本，确认收益再动。
- 保真：bit-identical（栅格逻辑不变，只延生命周期）。

### 7.3 CPU clip bbox-scope mask
- 每个 clip 层级全栅格 `pw*ph` mask → bbox 大小 mask + origin。
- 保真：bit-identical（mask 内容同，仅尺寸/原点改变，相交逻辑适配）。
- 风险：mask-origin 数学需仔细，错则覆盖错位。

### 7.4 解释器算符分发
- 消灭每算符 `String` 分配 + `op.as_str()` 匹配 → 借用 `&str`/enum 分发表。
- 较大重构（6310 行 interpreter）；保真：bit-identical（只改分发机制）。

### 7.5 延后项
- 函数栈分配（非逐像素，收益有限）、跨页 image 纹理缓存、parser 微优化。
- 视前面几项度量结果再定是否上调。

---

## 8. 实施里程碑顺序

1. **M0 基准设施**：建 `zpdf-benches` crate + criterion；落 §4.3 微基准与 §4.4 端到端矩阵；
   跑出**基线**数字存档（before）。
2. **M1 CPU 字形缓存**：按 §5 实现；跑 §6 全套验证；存 after 数字。
3. **M2 GPU 资源池**：§7.1；多页吞吐度量。
4. **M3 跨页 GPU atlas**：§7.2（视 M2 度量）。
5. **M4 CPU clip bbox-scope**：§7.3。
6. **M5 解释器分发**：§7.4。
- 每个 M 都 gate 在“速度达标 + 保真契约满足 + 基准无回退”三者之上。

---

## 9. 风险与回滚

- **亚像素漂移超标**：若 §6.2 显示漂移 > 1/16 px 或出现结构性差异，调高 bin 数（16×16）
  或回退到“path 缓存 + 重栅格化”（保 strict）。
- **缓存拖慢非 text 页**：缓存不命中应零成本回退；若引入额外判定开销导致 vector/image 页
  回退，加 `ZPDF_CPU_GLYPH_CACHE=0` 关闭路径验证。
- **GPU parity 恶化**：若 §6.3 新基线显著劣化，重新审视 GPU atlas 的亚像素行为是否需对齐。
- 每项改动独立 commit，便于二分回滚。

---

## 10. M1 实测发现（2026-08-10）— 决策待定

M1（CPU 字形缓存）已实现并完整 bench。**受控 A/B**（同一 M1 二进制，
`ZPDF_CPU_GLYPH_CACHE=0` 关缓存 vs 默认开，criterion `--baseline`）结果：

| 配置 | test8(拉丁文本) | test6 | test3 | test10 | testpdf-ai(中文,205ms) | zzztest2 |
|---|---|---|---|---|---|---|
| 8×8 bin（§2.7 批准，≤1/16px） | **+15% 慢** | +45% 慢 | +24% 慢 | +32% 慢 | 持平 | 持平 |
| 4×4 bin（≤0.125px，达标） | +19% 慢 | +38% 慢 | +13% 慢 | +24% 慢 | 持平 | 持平 |
| 1×1 bin（无亚像素，≤0.5px） | **−11% 快** | +4% 慢 | −4.5% 快 | 持平 | 持平 | 持平 |

**结论**：
1. **8×8（批准设计）净负** —— 亚像素分桶把缓存命中率压垮（每个亚像素是独立条目，
   真实文本很少重复同一 (字形, 亚像素)）。
2. **不存在“既净正又满足 ≤0.125px 漂移”的 bin 数**。1-bin 净正但漂移 0.5px（4× 越界），
   且仅对拉丁文本 +11%、绝对值很小。
3. **最慢的页（中文 testpdf-ai 205ms）任何配置都不受益** —— 中文字形重复率低，
   缓存从根本上帮不上（每个字唯一，无命中）。
4. blit 方案本身没问题（1-bin 拉丁页确实更快）；问题是亚像素分桶 + CPU 上
   `draw_pixmap` 的小字形 per-call 开销。

**额外发现（bench 设计缺陷）**：当前 bench 的 `load_page` **省略了 `.with_annotations`**，
导致测得的 DL 比真实 CLI 简化 —— 同一页 test8：bench 渲染 9ms，CLI `--stats`
渲染 **179ms**（20×）。真实 DL 含注释字形/表单字段，渲染开销远大于 bench 所示。
**任何渲染优化在修正 bench 前都无法被可信验证。**

**真实瓶颈重估**：CLI `--stats` 显示 test8 渲染 179ms（含注释），而 parse+interpret+PNG
另计。需要修正 bench 后才能确定渲染 vs 解析的真实占比。

**待决策**（见与用户的 grill）：A 先修 bench 再重评 M1；B 1-bin 小幅拉丁收益落地上线；
C 放弃 M1 转向解析/解释器（真实瓶颈可能在 parse+interpret）；D 放弃 M1 转 GPU 资源池。

### 10.1 bench 修复 + M1 重评（2026-08-10）

按用户决策“先修 bench 再重评 M1”，`load_page` 已补 `.with_annotations`/
`.with_colors`/OC/output-intent，DL 现与 CLI 一致（test8 实测 843 glyph runs，
与 CLI `--stats` 吻合）。并加 `ZPDF_BENCH_DEBUG=1` 打印 DL 命令分解。

**真实 DL 命令分解**（修正了错误语料标签）：

| 语料 | glyphs | images | clips | bench 暖渲染 | CLI 冷渲染 | 诊断 |
|---|---|---|---|---|---|---|
| test8 | 843 | 1 | 1 | 9 ms | 179 ms | 冷/暖 20× |
| test6 | 276 | 0 | 0 | 12 ms | 256 ms | 无图 → 纯轮廓解析 |
| testpdf-ai | 42 | 13 | 37 | 204 ms | 204 ms | clips+images，非文本 |
| test10 | 346 | 0 | 0 | 25 ms | — | 实为文本页（标签错） |
| zzztest2 | 0 | 2 | 2 | 26 ms | — | 图像页（标签错） |

**M1 在修复后 bench 上的 A/B**（8-bin vs nocache）：仍**全面净负**（test8 +22%、
test6 +44%、test10 +33%）。bench 修复未改变 M1 判决——这些页无注释字形，
且深层问题（分桶杀命中率 + 暖渲染本就只 9–26ms）成立。

**真正的单页瓶颈 = 冷字体轮廓解析**：test8 暖 9ms vs 冷 179ms（20×）；
test6 **零图像**仍冷 256ms vs 暖 12ms（21×）→ 冷开销是字形轮廓提取（CFF/TrueType
charstring/glyf 解析），**非栅格化、非图像**。M1 缓存的是栅格化，碰不到这层。
暖渲染已 9–26ms（小）；冷渲染被轮廓解析主导，M1 无关。

**结论**：M1（CPU 字形栅格缓存）对两个目标均无实质收益——单页 CLI 延迟被冷轮廓解析
主导（M1 不触及），批量暖渲染本就快（M1 仅省 ~1ms）。**M1 判死**。下一步见与用户决策。

### 10.2 M1' 字形轮廓/Face 缓存（2026-08-10）— 假设证伪

用户选“调查冷轮廓解析”。代码定位 `LoadedFont::glyph_outline`（zpdf-font/lib.rs:577）
**每次调用都重新 `ttf_parser::Face::parse`**（未缓存）。初步假设：Face::parse 是成本，
缓存 Face → 估计 −85% 暖渲染。

**实测证伪**：
- **轮廓缓存原型**（renderer 内 `HashMap<(font,gid), Option<GlyphOutline>>`，跳过重复字形的
  outline_glyph）：test8 **−8%** 暖。
- **Face 缓存**（`FontFaceCache<'a>`，每字体解析一次 Face 复用；已实现于 zpdf-font + render-cpu）：
  test8 **仅 −4%** 暖，testpdf-ai **+0.7%**（略伤慢页）。

**根因**：`Face::parse` 其实很便宜（ttf-parser 懒解析表，~0.4µs/次）；真正的成本是
**`outline_glyph`**（~10µs/字形，走字形轮廓）。Face 缓存不跳过 outline_glyph；轮廓缓存才跳过
（但仅对重复字形）。所以：
- 缓存只帮“重复”部分——拉丁文本 −4~8%，中文（低重复）~0。
- 冷 CLI 的 179ms = outline_glyph × 843（冷 CPU 缓存），是**基本工作量**，缓存无法绕开
  （每个唯一字形必须解析一次）。

**慢页真相**：testpdf-ai（204ms）= 42 字形 + **37 裁剪 + 13 图像**。瓶颈是**裁剪掩膜**
（37 × 全栅格 2.2Mpx mask ≈ 81M 像素操作），**非字形**。字形缓存对慢页无益甚至略有害。

**结论**：字形层缓存（Face 或 outline）净收益边际（拉丁暖 −4~8%，慢页中性偏负）。
真正高影响目标是**裁剪掩膜**（短名单 #3，CPU clip bbox-scope）——直接命中慢页的 204ms。

Face 缓存代码已实现（zpdf-font `FontFaceCache` + render-cpu 接入），bit-identical，clippy/test
通过。是否保留待用户决策（−4% vs 跨 crate API 成本）。

### 10.3 分阶段实测（2026-08-10）— 瓶颈再确认（直接证据）

加临时 `ZPDF_RENDER_PROF=1` 探针，`end_page` 打印 fill/stroke/glyph/image/clip 五阶段
墙钟。结果（直接 CLI 跑，非 bench）：

| 页 | 总 | fill | stroke | **glyph** | **image** | **clip** |
|---|---|---|---|---|---|---|
| testpdf-ai (3663ms) | 3663 | 0 | 0 | 0 | **3573 (98%)** | 89 |
| test8 (124ms) | 124 | 0.15 | 0 | **119 (96%)** | 4 | 0.1 |
| test6 (155ms) | 155 | 0 | 3.4 | **151 (98%)** | 0 | 0 |
| test10 (232ms) | 232 | 0 | 0 | **232 (100%)** | 0 | 0 |

**两个独立瓶颈**（不是 clip）：
1. **testpdf-ai 是图像绑定**（3573ms，98%）：13 张图经双线性 `draw_pixmap` 缩放到 2.2Mpx；
   37 clip 仅 89ms——clip 假设再次证伪。
2. **test8/test6/test10 是字形绑定**（96–100%）：outline_glyph + fill_path。Face 缓存目标正确，
   但每页唯一字形多（843/276/346），缓存只省重复部分 → 暖 bench −4~8%，CLI 冷无法绕开
   基本轮廓工作量。

**结论修正**：真正的高影响目标是
- **字形层**：提高 outline_glyph/小字形 fill_path 的效率（非缓存——基本工作量），
  或真正降低单字形栅格成本（亚像素 raster 缓存被命中率/漂移绑死，已证伪）。
- **图像层**：testpdf-ai 的 3573ms 是双线性 `draw_pixmap` 缩放——可能预生成 mipmap /
  对齐降采样 / 跳过完全遮挡图能省。**这是迄今最大单一可优化点**（一页省 3s+）。

### 10.4 逐图像实测（2026-08-10）— 瓶颈最终定位

加 `ZPDF_RENDER_PROF=1` 逐图像探针（已移除），testpdf-ai：

| 图 id | src | →dev | fx | fy | downscale? | 耗时 |
|---|---|---|---|---|---|---|
| 0 | 1548×871 | 2000×1125 | 1.29 | 1.29 | 否 | **1691ms** |
| 1 | 768×432 | 2000×1125 | 2.60 | 2.60 | 否 | **1837ms** |
| 6 | 768×432 | 2000×1125 | 2.60 | 2.60 | 否 | **1848ms** |
| 2-5 | 768×432 | (离屏) | — | — | 否 | ~0.02ms（被 tiny-skia 裁掉） |

**最终定位**：testpdf-ai 的 3.6s = **3 张双线性上采样全页背景图**（fx 1.29–2.60）。
每张 ~1.8s = 2.25Mpx × ~800ns/px 的双线性采样基本工作量。
- box-downscale 缓存不触发（仅 fx<0.5 缩小才走；这些是放大）。
- 37 clip 仅 89ms——clip 假设证伪。
- 图 id 2-5 离屏 → 0.02ms，证明开销全在可见的 3 张大图。

字形页（test8/test6/test10）确认：test8 唯一图 81×61 1:1 = 3.77ms；test10 无图。
→ 字形开销 = outline_glyph + fill_path 基本工作量。

**最终结论**：CPU 单页两大瓶颈均为**基本采样/栅格工作量**，
- 图像：3× 大背景图双线性上采样（5.4s）。可优化方向：跳过被后图完全遮挡的图、
  近 1:1 用 nearest/双线性混合、降采样到目标分辨率再上采样（但放大无损难）。
- 字形：outline_glyph × N（基本轮廓工作量）。缓存仅省重复（已证伪亚像素缓存净负）。

无“低垂果实”——CPU 优化要么动图像采样策略（风险中），要么接受现状转 GPU（M2）。
所有探针代码已移除，render-cpu 回到干净状态；仅 M0 bench + 本文档保留。

### 10.5 图像遮挡剔除——可优化点确认（2026-08-10）

逐图像 + transform 探针揭示 testpdf-ai 是**2×2 图像拼贴**：

| id | src | tm(e,f) | 象限 | 耗时 |
|---|---|---|---|---|
| 0 | 1548×871 | (0,0) | 左上 | **1704ms** |
| 1 | 768×432 | (0,0) | 左上（同 id 0） | **1804ms** |
| 6 | 768×432 | (0,0) | 左上（同 id 0/1） | **1802ms** |
| 2 | 768×432 | (-960,-540) | 左下/离屏 | 0.03ms |
| 3-5 | 768×432 | 各象限 | 拼贴 | 0.02ms |

**关键**：id 0、1、6 三张**完全不透明（a=1）、无 blend group**、transform 完全重合
（同一左上象限 960×540），按 DL z-order 依次叠加。id 6 完全覆盖 id 0 和 id 1 的设备
footprint → 后两者被完全遮挡。

**bit-identical 剔除条件**（待 grill 确认）：
1. 后续 draw 完全不透明：`alpha=1.0`，非 overprint，非 blend group 内（Normal/无 mask）。
2. 后续 draw 的设备 bbox ⊇ 前图像设备 bbox（含相同 clip 状态）。
3. 两图之间无内容需要前图可见（无透明叠加依赖）——即中间命令全是被同样覆盖的 draw，
   或无命令。
4. clip 状态一致（前图的 clip 不能比后图更松，否则前图在更松区域可见）。

**预期收益**：跳过 id 0 + id 1（被 id 6 覆盖）= 省 ~3.5s，单页 3.6s → ~0.2s。
这是迄今最大的单一优化，且 fidelity-neutral（被覆盖的像素本就被覆盖）。

**实施位置**：`CpuRenderer::execute` 收到 `DrawImage` 时，需要前瞻 DL 后续命令——
但 `execute` 是单命令流。需在 `render_display_list`/`begin_page` 前做一次 DL 预分析
（标记可跳过的 image 命令），或在 execute 内维护“已见的不透明覆盖”状态。后者更简单：
维护一个“被不透明覆盖的设备矩形集合”，新 image 若完全落在已覆盖区域则跳过。

### 10.6 图像遮挡剔除——实测证伪（2026-08-10）

实现 conservative same-clip opaque-cover 剔除（`covered_rects` + `image_device_bbox`，
bit-identical 验证通过：`zpdf compare` 0 差异像素）。但**实测无加速**（ON/OFF 均 ~8.7s）。

加 clip/blend 深度探针揭示根因——3 张昂贵图在不同 clip/blend 上下文：

| id | 耗时 | clips | blend | 上下文 |
|---|---|---|---|---|
| 0 | 1697ms | 0 | 0 | 顶层背景（任何后续图之前，但随后 3×PushClip 清空 cover） |
| 1 | 1782ms | 2 | 1 | 第 1 个透明组内（Normal blend + soft mask），clip 深度 2 |
| 6 | 1867ms | 4 | 1 | 第 6 个透明组内，clip 深度 4 |

**为什么剔除无效**：
1. id 0 的 cover 被紧跟的 3× PushClip 清空（conservative 设计：clip 改变可见性，
   不再描述完整设备矩形）→ 没有后续图能被它剔除。
2. id 1（clip=2）和 id 6（clip=4）处于**不同 clip 深度** → 即便不清空，
   `depth >= cover.clip_depth` 要求阻止跨深度剔除（cover 在浅深度不保证在更深 clip 下仍覆盖）。
3. 三图虽 transform 重合（同左上象限），但各自独立的 clip/blend 组使它们无法互相剔除。

**结论**：conservative same-clip 剔除对本页（也是唯一图像密集慢页）无效——昂贵图
处于独立透明组+不同 clip 深度，无同上下文的覆盖关系。更激进的剔除（跨 clip/blend）
会牺牲 bit-identical 保证（透明组的 soft mask 使覆盖关系不可静态判定）。

**最终判定**：CPU 图像遮挡剔除对真实语料无效。代码已回退（`render-cpu` 回到 HEAD）。
CPU 两大瓶颈（字形 outline、图像双线性上采样）均为基本工作量，无低风险优化。
→ 整体结论：CPU 渲染无可行优化空间（在不放宽保真契约下），转向 GPU（M2）。
