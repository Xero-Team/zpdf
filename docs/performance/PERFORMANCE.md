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

## 12. M2 GPU 资源池——假设证伪（2026-08-10）

启用 `gpu_render` bench（`--features gpu-render`），先取 baseline 再设计池。
数据再次推翻假设：

**gpu_render bench**（复用单个 `WgpuRenderer`，设备初始化已摊销）：

| 页 | bench wall | 内容 |
|---|---|---|
| test8 | 170ms | 843 字形 |
| test6 | 166ms | 276 字形 |
| test3 | 165ms | 110 字形 |
| image-test10 | 170ms | 346 字形 |
| zzztest2 | 111ms | 2 图（最小 DL） |

**关键**：所有页 ~165ms **固定下限**，与 DL 大小无关（CPU 同页 4–205ms 波动）。
→ 不是 per-page 分配开销，是 **host↔device 同步停顿**。

**CLI `--stats` 证实**（`--backend wgpu`）：

| 页 | wall | **gpu pass** | 比值 |
|---|---|---|---|
| test8 | 631ms | **0.04ms** | 15,000× |
| test6 | 547ms | **0.03ms** | 18,000× |
| test3 | 478ms | **0.03ms** | 16,000× |

**GPU 实际工作 < 0.05ms**（GPU 几乎空闲）；wall = 478–631ms 全是**同步等待**：
`map_and_strip` 里 `device.poll(wait_indefinitely)` 阻塞到 GPU 执行 + readback 完成。
GPU 太快，host 一直在 idle-wait 同步。

**结论**：M2 资源池**不会显著改善单页延迟**——瓶颈是 `device.poll` 阻塞同步，
不是 per-page 分配。资源池仅帮批量多页（连续排队），但前提是**不每页同步**。
真正优化是 **pipelining**：第 N 页 readback 等待时提交第 N+1 页，或 async map 替代
阻塞 poll。但这是比资源池更大的架构变更（`render_display_list` API 同步，
pipelining 需新 API）。

**待用户决策**：A 实施 pipelining（大改、高收益、需 API 变更）；
B 实施资源池（小改、仅省分配几 ms、batch 微益）；
C 接受 GPU 现状，本会话到此。

### 12.1 M2 → pipelining 设计（2026-08-10）

用户选 pipelining。瓶颈确认：`map_and_strip` 和 `timer.resolve_ns` 各自
`device.poll(wait_indefinitely)` 阻塞——每页 2 次同步等待，GPU 通道仅 0.04ms。

**pipelining 原理**：`device.poll` 等待**所有** pending GPU 工作 + 映射。所以
提交第 N 页 + 启动其 readback map，再提交第 N+1 页，一次 poll 等两者。第 N+1 页
的提交与第 N 页的 GPU 执行重叠。0.04ms GPU pass + ~165ms sync-wake →
多页吞吐从 N×165ms 降到 ≈ 1×sync-wake + N×0.04ms（理想）。

**API 契约**（待 grill 确认）：
- 新增 `WgpuRenderer::render_pages_batch(&mut self, pages: &[&DisplayList], scale) -> Result<Vec<GpuTexture>>`：
  逐页 `begin_page`/`execute`/record-encoder，但**不立即 map_and_strip**；
  提交第 N 页后启动其 readback map_async，立即提交第 N+1 页；最后统一 poll + 收集。
- 保持 `render_display_list` 单页同步 API 不变（CLI/viewer 单页仍用旧路径）。
- bit-identical：pipelining 只改变**提交/收集时序**，不改绘制内容；每页仍是独立
  render pass + 独立 readback buffer。需 `zpdf compare` 逐页验证 batch vs single 一致。

**正确性边界**：
- 每页需**独立 readback buffer**（不能复用——前页 map 未 unmap 时后页不能写同 buffer）。
  → 这正是“资源池”的部分：readback buffer 要 per-inflight，而非单 buffer 复用。
  故 pipelining 实际是 **双缓冲 readback**（2 个 buffer 交替）+ 延迟 poll。
- `PageTarget` 纹理能否跨页复用？submit 后 GPU 持有，但下一页 begin 可重用（若尺寸相同）。
  保守起见先**不复用纹理**，只延迟 poll + 双缓冲 readback（最小正确性风险）。
- `timer.resolve_ns` 的第二次 poll 也需纳入流水线（或在 batch 末尾统一）。

**预期收益**：多页（如 split/convert）从 N×165ms → ~165ms + N×0.04ms。
单页无变化（仍同步）。对“批量吞吐”目标是大胜，对“单页延迟”无影响。

### 12.2 pipelining 实测——热态净负（2026-08-10）

实现完成：`end_page` 拆为 `finalize_and_submit`（record+submit+start_readback，不阻塞）
+ `collect_readback`（poll+strip+resolve_ns）；`PageTarget::map_and_strip` 拆为
`start_readback`/`finish_readback`；新增 `render_pages_batch`（depth-2 双缓冲）；
`take_context` 在 `pending` 非空时拒绝。bit-identical 测试通过（batch-of-one 与
multi-page 像素与 single render 完全一致）。

**速度实测（关键修正）**：先前的 165ms/页是**冷态**（首次设备初始化）。
热态（设备已初始化、renderer 复用）单页仅 **14–72ms**（中位 ~32ms）。8 页对比：

| 页 | 8×热单页 | 8 页 batch (pipelined) | 结果 |
|---|---|---|---|
| test8 | 8×29ms ≈ 256ms | 390ms | **batch 慢 140ms** |
| test6 | 8×35ms ≈ 280ms | 413ms | batch 慢 133ms |
| test3 | 8×30ms ≈ 240ms | 380ms | batch 慢 140ms |

**pipelining 在热态净负**：depth-2 的开销（管理 2 个 readback buffer、交错
submit/collect、多次 poll）超过它省的同步等待——而热态单页同步仅 ~32ms
（非先前误判的 165ms）。先前“3× 加速”是与**冷态** baseline 比，但热-热对比
（公平对比）pipelining 慢 ~50%。

**根因**：165ms baseline 含一次性设备初始化；真实每页同步等待 ~32ms。
pipelining 的收益假设是“每页 165ms 同步”，但热态每页仅 32ms，pipelining 的
管理开销 > 它省的同步。depth-2 不够深到摊销；更深则内存暴涨。

**最终判定**：M2 pipelining 在热态净负，不应上线。代码已实现且 bit-identical，
但速度不达预期。是否保留 API（供未来更深的 pipeline / 不同负载）或回退，
待用户决策。

---

## 13. 测量阶段实测（2026-09-12）— 用测量取代推断

上一轮（§10–12）的六个结论**全部建立在推断而非测量之上**：探针是临时 env-var 代码、
跑完即删；语料标签手写且**有错**；只有 150 DPI 一个点；`encode` 阶段从未进入任何数字。
本轮先补测量基建（§13.1），再取数（§13.2），**结果推翻了上一轮的多项结论**（§13.3）。

### 13.1 测量基建（已落地）

| 项 | 内容 |
|---|---|
| 五阶段外部计时 | `parse` / `interpret` / `render` / `encode` / `total`，各一个 criterion group（`benches/stages.rs`） |
| 页内 7 类分解 | 后端 opt-in 类型化 stats：`outline-parse`/`glyph-raster`/`fill`/`stroke`/`image`/`clip`/`soft-mask` + 9 个确定性计数器，带断言测试（`zpdf-render::StageStats`、`zpdf-render-cpu`） |
| interpret 分解 | `zpdf-content::InterpretStats`（shading 栅格化只在 interpret 可见，后端收到的是普通 image） |
| DPI 曲线 | `{96, 150, 300}`，区分填充率绑定与几何绑定 |
| 冷/暖两轴 | (a) **渲染器**级：fresh vs reused `CpuRenderer`；(b) **进程**级：`cli_process` 直接 spawn CLI。二者含义不同，不可混称"冷" |
| 负载分类 | 由 DL 组成**测量**得出（`Composition`：三类工作量折算为可比的 device-px 覆盖），不再手写标签 |
| 并行 ceiling | `benches/batch.rs`：rayon（仅 dev-dep）页级并行，1/2/4/8/16 线程，每任务独立 open |
| 语料完整性 | 合成语料（9 个 / 22 KB）+ 生成器入 git；真语料与 failed 大文件走 manifest **硬失败**校验（缺失/哈希不符即报错，不再静默跳过） |
| 回退门禁 | 检出 JSON baseline（机器指纹 + 语料 manifest 哈希 + git commit）+ `baseline --compare` 按 +5% 阈值非零退出；CI 只跑确定性计数器 + 宽松墙钟上限 |
| GPU 有效性 | 适配器身份记录与校验（本机有 2 个**虚拟显示适配器**，`request_adapter` 静默选一个）；wall 与 gpu_pass 双数；**新增无 readback 的 submit-only 路径**（viewer 消费模式） |

### 13.2 实测数据（release，150 DPI，Release CLI，RTX 5080 / Vulkan）

**五阶段分解（ms）**：

| 页 | 类 | parse | interpret | render | encode | 说明 |
|---|---|---|---|---|---|---|
| test8 | text | 10.3 | 3.8 | 9.8 | 2.8 | outline-parse 1.0 vs glyph-raster 7.2 |
| test1 | text | 1.4 | **15.7** | 14.6 | 2.3 | **interpret > render** |
| test10 | text | 20.8 | 2.2 | 19.7 | 3.6 | parse 21 ms 出乎意料 |
| test11 | vector | 0.2 | 10.2 | 22.6 | 3.3 | 分类=vector 但 66% 时间在 glyph |
| test2 | vector | 1.4 | 4.3 | 23.3 | 4.7 | |
| test7 | vector | 1.1 | 6.4 | 50.8 | — | clip 占 35% |
| testpdf-ai | image | 4.6 | **78.5** | 209 | — | soft-mask 占 74% |
| test12 | image | 5.4 | **397** | 438 | — | interpret 与 render 同量级 |
| test4 | image | 8.6 | 4.3 | 18.4 | — | image 占 91% |

**页内 7 类分解（%）**：

| 页 | 总 | glyph | image | soft-mask | clip | fill |
|---|---|---|---|---|---|---|
| test8 text | 9.6 | **85** (parse 1.0 / raster 7.2) | 1 | 0 | 1 | 0 |
| test10 text | 19.5 | **94** | 0 | 0 | 0 | 0 |
| test11 vector | 24.9 | 66 | 0 | 0 | 2 | 22 |
| test7 vector | 51.3 | 24 | 0 | 13 | **35** | 23 |
| testpdf-ai image | 223 | 1 | 18 | **74** | 2 | 0 |
| test12 image | 438 | 3 | 21 | **69** | 2 | 0 |
| test4 image | 18.4 | 0 | **91** | 0 | 3 | 0 |

**进程级 vs 进程内**：同页 test8 — 进程内复用渲染器 9.84 ms、fresh 渲染器 9.89 ms、
**CLI 进程 91.3 ms**（9.3×）。

**GPU 双数（150 DPI，ms）**：

| 页 | readback | submit-only | 比值 | CPU 对照 |
|---|---|---|---|---|
| test8 text | 8.65 | 2.67 | 3.2× | 9.8 |
| test11 vector | 9.51 | 4.64 | 2.0× | 22.6 |
| test7 vector | 44.9 | — | — | 50.8 |
| testpdf-ai image | 24.1 | — | — | 209 |
| test12 image | 25.0 | — | — | 438 |

### 13.3 被推翻的结论

1. **"testpdf-ai 3.6 s，是最大单一优化目标"**（§10.3/§10.4）→ 该页 release/150 DPI 实测
   **209 ms**。3.6 s 是**带探针的**测量（§10.3 自己标注 `ZPDF_RENDER_PROF`），§10.4 的
   "3 张双线性上采样图 = 3.5 s"拟合的是探针开销，不是生产成本。**上一轮据此判定的
   "CPU 无优化空间"其前提不成立。**
2. **"冷开销是字形轮廓提取"**（§10.1）→ 实测 outline-parse : glyph-raster = **1.0 : 7.2**
   （test8），outline 提取至多占 glyph 时间 12%。上一轮 M1'（轮廓/Face 缓存）方向正确但
   收益上限被高估；其"净收益边际"的结论恰好与实测一致。
3. **"慢页瓶颈是图像双线性上采样"**（§10.4/§10.6）→ 两页最慢页的主导桶都是
   **soft-mask 合成**（74% / 69%），图像只占 18–21%。上一轮的图像剔除方向（已证伪）
   瞄错了 70% 的时间。
4. **"clip 假设证伪，clip 只占 89ms/3663ms"**（§10.3/§10.4）→ 仅在 testpdf-ai 成立；
   test7 上 clip 占 **35%**（17.9 ms / 51.3 ms）。
5. **"parser/interpret 相对渲染很轻"**（§3.1/§3.2）→ 从不成立的假设。实测 interpret 占
   流水线 10–47%：test1 的 interpret（15.7）**高于** render（14.6）；test12 的 interpret
   **397 ms**，与 render（438 ms）同量级，且 **DPI 无关**（388/397/396 @ 96/150/300），
   是几何绑定工作。**上一轮完全没测过这个阶段。**
6. **"进程内冷 179ms vs 暖 9ms，20×"**（§10.1）→ 分解为两轴后：**渲染器**级 fresh vs reused
   差异在噪声内（9.89 vs 9.84 ms），所以 20× **不是**渲染器生命周期效应；**进程**级
   CLI/进程内 = 9.3×，来自进程启动 + parse + interpret + encode + PNG 写盘。
7. **GPU "瓶颈是 device.poll 同步，资源池无收益"**（§12）→ 新增的 submit-only 路径
   （**无任何同步**）仍有 **2.67 ms/页**，即**无同步时的成本是 host 端录制+分配**。
   上一轮因为 wall 被同步主导而否定了资源池（M2）；**去掉同步后，per-page 分配正是成本本身**，
   M2 值得重估。
8. **`encode`（PNG）不是瓶颈**：实测 2.3–4.7 ms/页（流水线的 3–13%）。C2 候选**排除**。

### 13.4 语料与方法论修正

- 语料标签由**测量**取代手写：`test8` 分类为 **text**（glyph 覆盖 349k device px vs 图像
  仅 4811 px —— 上一轮把这张 1163 字形的页当成 "text-heavy" 只是碰巧对了）；`zzztest/2`
  为 **image**（2.9 M px，0 字形）。
- **分类（覆盖率）≠ 耗时**：test11 按覆盖率为 vector（17.5 M device px）却把 66% 时间花在
  glyph 上。两者都要看。
- 旧 6 页集**完全没有 vector 页**；新 9 页集（每类 3 页，含各类**最重页**）覆盖 text/vector/image。
- 单页选择采用"**每类最重 + 路径序补足**"：否则 29 M px 的 testpdf-ai 会因路径序第 5 而落选。
- 300 DPI 页面成本 ×4，故采样数**按实测单次成本**选择（≥200 ms → 10 samples），
  否则 criterion 会把 3 s 预算静默变成 40 s（实测单案例 39 s）。

### 13.5 候选重排（本轮测量后的排序）

| 排名 | 候选 | 依据 | 状态 |
|---|---|---|---|
| **1** | **soft-mask / blend-group 合成**（新） | 两个最慢页的 **69–74%**；上一轮从未测量 | **已落地（§14，−39%/−51%）** |
| **2** | **interpret 阶段**（新） | 占流水线 10–47%；test12 **397 ms** 且 DPI 无关；其中 shading 栅格化只在 interpret 可见 | 新候选（**现在的第一名**） |
| **3** | GPU host 端 per-page 分配（= 重估 M2 资源池） | submit-only 无同步仍 2.67 ms/页 | 复活 |
| **4** | 图像重采样 | test4 的 91%、慢页的 18–21% | 确认（非主导） |
| **5** | clip（clip-heavy 矢量页） | test7 的 35% | 复活（限该类） |
| 6 | 页级并行（C1） | ceiling **未测得**（batch target 本轮被 GPU OOM 中断） | 待测 |
| — | ~~PNG encode~~ | 2.3–4.7 ms | **排除** |
| — | ~~outline 提取缓存~~ | 上限 12% of glyph time | 降级 |
| — | ~~渲染器生命周期/冷启动~~ | fresh ≈ reused | **排除** |

### 13.6 本轮顺带修掉的缺陷

- **submit-only 路径 GPU OOM**：无 readback 即无 `device.poll`，紧密循环堆积未回收提交，
  数百页后设备报 Out Of Memory（4 GB 级 GPU 实测）。已修为非阻塞 `PollType::Poll` 抽干，
  并加 300 次迭代回归测试。
- **bench 静默测空**：缺失语料原先只打一行 stderr 然后跳过、退出码 0；已改为默认硬失败。
- **bench 重复哈希**：每个 group 都重新校验 331.7 MiB 语料（5×）；已按进程缓存一次。
- **`cli_process` 曾测 debug CLI**：debug 二进制单页 ~15 s，会与 release 数字并列却不可比；
  已改为仅接受 release，否则带说明跳过。

### 13.7 停机点

按约定（决策 11）到此**停机**：测量基建 + 基线 + 候选排序已产出，**未改动任何渲染核心**。
下一步需就"做候选 1（soft-mask）还是候选 2（interpret）"重新 grill 后再动手。

### 13.8 候选 1 的子桶测量（2026-09-12）— 目标定位

soft-mask 桶占两个最慢页总时间的 69–76%，但"soft-mask"作为一个数字无法指出该打哪里。
加四个**归属**子桶后（`mask_render` / `mask_reduce` / `mask_fold` / `mask_composite`）：

| 页 | soft-mask | **composite** | render | reduce | fold | 未计入 |
|---|---|---|---|---|---|---|
| testpdf-ai（12 组） | 152.3 ms | **82.4（54%）** | 22.0 | 5.7 | 29.6 | 12.5（8%） |
| test12（18 组） | 282.6 ms | **143.7（51%）** | 69.7 | 39.0 | 18.6 | 11.6（4%） |
| test7（1 组） | 6.5 ms | **6.5（100%）** | — | — | — | — |

**最大单项 = 每个混合组一次整页 `draw_pixmap`**（组内容以组的混合模式 + 常量 alpha 合成回
backdrop），在两页上约占**整页时间的 28–40%，集中在一个调用点**。mask 的渲染/归约/折叠
加起来反而更小。

**同时修正了一个本改动自己提出的错误断言**：子桶最初被写成 `soft_mask_ns` 的**划分**，并配了
`sum == total` 的测试。实测否定之 —— 第一版只归属了约 40%，补上 composite 后仍只覆盖
92–96%，余下是 `shift_plane` 与缓存维护。文档与测试已改为**归属**语义（`sum ≤ total`），
并把这次否证记录下来，而不是悄悄删掉。

## 14. 方向 1 实施方案（bbox-scope 混合组合成）— **已落地**（2026-09-12）

### 14.1 目标

把 `pop_blend_group` 里那次整页 `draw_pixmap` 收窄到组的**实际绘制范围**。test12 的
18 个组里多数只覆盖页面一小块。

### 14.2 可行性依据

tiny-skia 的 `draw_pixmap` 对所有混合模式都做正确的 Porter-Duff alpha 合成 ——
**源 alpha = 0 处结果等于 backdrop**（倍数/滤色/HSL 族同理）。因此只要 bbox 是组"画过的
东西"的**保守超集**，区间外不合成即**像素等价**。

形式化地：通用混合式 `r = s·(1−da) + d·(1−sa) + B(s,d)·sa·da` 在 `sa = 0` 时退化为 `d`。
这条不是"看起来对"——`scoped_composite_matches_full_raster_for_every_blend_mode` 对
**全部 16 种混合模式 × {alpha 1.0, 0.25} × {isolated, 非 isolated} × {scale 1, 2}**
逐一做了字节比对。

### 14.3 实现要点（含落地时相对本节的偏离）

落地版本与下面的计划有四处不同，逐条记录，而不是让计划与代码各说各话：

1. **收窄手段：矩形 `fill_rect` 而不是 scratch 拷贝。** 计划里"整页 `draw_pixmap` 换成
   scratch pixmap + 局部合成"要 3 遍内存搬运（拷入 backdrop、合成、拷回），实际做法是
   直接构造 `Pattern` shader + `Pixmap::fill_rect(region, …)` —— 这正是 tiny-skia 的
   `draw_pixmap` 的内部实现（它就是 `fill_rect(src.size().to_int_rect(x,y), pattern_paint)`），
   只是把 blit 的矩形从"源的全幅"换成 region。**一遍、零拷贝、同一管线**，因此不存在
   "收窄后逐像素与整页不同"的可能。
2. **因此不需要"覆盖率 ≈ 1 走原路"的分支。** 局部合成永远不会比整页合成慢（同一次调用、
   更小的矩形），§14.4 里担心的"无谓开销"只剩下记账本身。test7（单组覆盖整页）实测
   6.62 → 6.55 ms，即零损失、零收益，与预测一致。
3. **Type3 不退回整页。** 计划把 Type3 列为"拿不准"；实际上 Type3 的字形内容在渲染器内部
   被解释成**普通设备空间路径**，与 outline 字形走同一个 `fill_path`，其 `bounds()` 同样
   可信。记录为事实并配测试（`…with_type3_glyphs_inside_the_group`），而不是继续保守。
   嵌套组同理：内外表面同尺寸同对齐，内层 region 在 pop 时并入外层即可。
4. **mask fold 也一并收窄。** 计划只提到 composite；折叠循环同样只处理 region（区间外
   组像素恒为 0，0 乘任何遮蔽值仍是 0，而合成也只读 region）。这一项不在 §14.5 的预测里，
   实测却占了收益的相当一部分（test12 fold 17.7 → 1.8 ms）。

具体规则：

- `CpuRenderer` 新增 `dirty: Option<PaintedBounds>`（半开区间 `[x0,x1)×[y0,y1)`），语义是
  **当前目标表面**的已绘制范围；`push_blend_group` 时把父表面的 region 存进 `BlendEntry`
  并把当前置空（新表面），`pop_blend_group` 时取回并与组的 region 求并。
- 每个绘制点保守并入，并与 `clip_bounds`（**本实现新增**：`current_clip` 的设备包围盒，
  随 clip 栈同进同出）相交：
  - 路径 → `tiny_skia::Path::bounds()` 再**向外扩 1px**（AA 边缘）；
  - 描边 → 路径 bounds 外扩 `miter_limit/2 × width + 1px`。计划里写的是 `2 × width`，
    落地时按 miter 的真实几何上界取，并由
    `scoped_composite_covers_a_miter_spike_past_the_path_bounds` 锁定：该测试里
    hairpin 的 miter 尖端到 x≈35，而 `2 × width` 只能覆盖到 x=32 —— 用宽度外扩会被它抓住；
  - 字形 → run 内**已变换**的各字形 bounds（不用 em-box 估计）；
  - 图像 → 变换后单位方形的四角 bbox；
  - Type3 → 同路径规则；
  - knockout / overprint → 它们通过整页 scratch + 整页合并实现，**直接标记整面**，并由
    `knockout_and_overprint_groups_are_conservative_and_sound` 断言"确实退回整面"。
- **保守优先**：非有限坐标 / 反向矩形 / 非有限外扩 → 退到"整面"；而**什么都没画**
  （`dirty == None`）→ **整个合成被跳过**（不是退到整面）。跳过这一条由等价测试反向验证：
  内容全部被 clip 剪掉的组，与"在整页上合成一个全透明组"必须字节相同。
- clip 已知的陈旧误差被显式绕开：`push_clip`/`push_clip_stroke` 与旧 clip 求交时只遍历
  路径 bbox（未按 miter 上界扩），因此**新的 clip 在其自身 bbox 之外仍可能非零**。
  `clip_bounds` 于是只由**新 clip 自身**的几何导出（不与旧 clip 求交）——否则记账会把这一段
  裁掉，成为真正的不安全收缩。代价是嵌套 clip 时 region 偏松，方向安全。


### 14.4 执行前必须先做的一步 —— **已做**（2026-09-12）

先量**组内容的实际覆盖率**：在 `pop_blend_group` 里统计 `group_pixmap` 的非透明 bbox 面积，
与 `页面积 × 组数` 比。为避免又一次临时探针，做成了**永久确定性计数器**
`StageStats::mask_composite_px`（只在开 stats 时累计，且在该桶计时窗口之外，不污染时间）。

**实测结果**：

| 页 | 组内容 bbox / (页 × 组数) | composite | 结论 |
|---|---|---|---|
| testpdf-ai | **8%**（2,250,000 / 27,000,000） | 82.4 ms | 12.5× 冗余 |
| test12 | **7%**（2,719,406 / 37,867,500） | 143.7 ms | 13.9× 冗余 |
| test7 | **100%**（2,174,960 / 2,176,200） | 6.5 ms | **反例**：单组覆盖整页，收窄无收益 |

**判定：方向 1 成立，且比预测更好** —— 两页最慢页的整页合成处理了 **12–14 倍**冗余像素
（预测假设是 20% 覆盖，实际 7–8%）。同时测量给出了**边界**：单组覆盖整页的页（test7）不受益。
（§14.4 当时据此建议"保留覆盖率接近 1 时走原路的分支"；落地时发现收窄本身不引入额外开销，
不需要该分支 —— 见 §14.3 第 2 条。）

**计数器由此从"探针"变成"结构"**：`mask_composite_px` 现在是**组内容 bbox 面积**（理想收窄
下界，仍只在开 stats 时统计），并新增两个确定性计数器（§14.7 会给数）：

- `mask_composite_region_px` —— 合成**实际处理**的像素数（Σ region 面积）。这是"做了多少
  工作"的诚实数字，取代原来的 `页 × 组数`。
- `mask_composite_leak_px` —— **落在 region 之外的已绘制像素数，恒应为 0**。它是整个优化的
  正确性判据：只有当 region 真包含组画过的每个像素时，"区间外源 alpha=0"的论证才成立。
  非 0 即后端丢像素。**测试里断言它**（确定性整数，正好是 CI 该管的东西），并用两次故意的
  变异验证过它会响 —— 去掉图像绘制的标记、把描边外扩退化成"宽度"，都被它/等价测试抓住。

### 14.5 收益预测（**已由 14.7 实测取代**）

composite 占两页总时间 28–40%；组内容实测只覆盖 **7–8%**，故理想情况下该项可降约 **×12–14**
→ test12 约 −128 ms（≈ −30% 整页）、testpdf-ai 约 −76 ms（≈ −37% 整页）。
**实测更快**（−39% / −51%）：预测没有算进"mask fold 也一起收窄"这一项。

### 14.6 验证协议 —— **已全部执行**（结果见 14.7）

1. `cargo fmt` + clippy + 全部单测 —— ✅ clippy 无告警；workspace 全部测试通过
   （新增 11 个测试：9 个等价/边界测试 + 1 个泄漏判据自检 + 1 个保守性断言）；
2. **同批页 before/after 字节级 PNG 比对** —— ✅ 计划里是 6 页，实际做了 **25 页**：
   计划那 6 页之外补上**全部有混合组的页**（test12 两页 / test7 / testpdf-ai 两页）
   与全部 9 张合成语料夹具。**25/25 字节完全一致**（sha256）；
3. `backend/cpu_stats` 复测 `mask_composite_ns` —— ✅ 同机 before/after，见 14.7；
4. CPU↔GPU oracle 套件 —— ✅ `gpu_softmask`（6）/ `gpu_overprint`（6）/ `gpu_acceptance`（3）
   全部通过。它们逐像素比对 CPU↔GPU，任何 CPU 侧像素变化都躲不过。
5. **（协议之外补做）failed 语料鲁棒性回归** —— ✅ 用 `tests/failed_run.sh` 跑全部 **618 个**
   对抗性 PDF（16 路并行、每个 20 s 上限），before/after **逐文件状态完全一致**
   （425 OK / 193 FAIL / 0 TIMEOUT / 0 PANIC，按路径排序后逐行相同）。193 个 FAIL 全是解析级
   错误（失配 xref / 无可用页 / 非 PDF），渲染根本没开始，故与本次改动无关。


### 14.7 实测结果（2026-09-12，同机 before/after）

**生产路径（criterion `backend/cpu_reused`，150 DPI，timing 关闭）**。同一代码库两次运行之间
无混合组的页本身就有 ±3–7% 漂移（实测：test11 两次 after 相差 6.4%），故只有远超噪声的数字
才算收益：

| 页 | before | after（两次） | 变化 |
|---|---|---|---|
| test12（18 组） | 382.0 ms | 228.2 / 233.0 ms | **−39% ~ −40%** |
| testpdf-ai（12 组） | 206.3 ms | 96.6 / 101.6 ms | **−51% ~ −53%** |
| test7（1 组，覆盖整页） | 49.5 ms | 48.9 / 50.3 ms | 噪声内（预测如此） |
| test8 / test10（text） | 9.52 / 18.94 | 9.53 / 19.00 | 噪声内 |
| test11 / test2（vector） | 22.38 / 23.47 | 23.87·22.33 / 24.62·24.14 | 噪声内（两次 after 自相差 6%） |
| test4（image，无组） | 19.65 | 18.39 / 19.03 | 噪声内 |

**阶段分解（`backend/cpu_stats`，timing 开启，同机 before/after，ms）**：

| 页 | total | soft-mask | composite | fold | render | reduce |
|---|---|---|---|---|---|---|
| test12 | 399.3 → **261.4** | 278.8 → **146.4** | 135.2 → **14.4**（×9.4） | 17.7 → **1.8** | 63.8 → 55.1 | 38.2 → 38.7 |
| testpdf-ai | 214.6 → **123.0** | 164.5 → **74.9** | 82.8 → **7.5**（×11.1） | 32.1 → **3.3** | 22.0 → 23.2 | 5.8 → 6.1 |
| test7 | 51.9 → 50.8 | 8.1 → 9.7 | 6.62 → 6.55 | — | — | — |

**覆盖率计数器（after，`backend/cpu_stats`）**：

| 页 | 组内容 bbox | 实际 region | 页 × 组数 | 收窄后占比 | leak |
|---|---|---|---|---|---|
| testpdf-ai | 2,250,000 | 2,259,384 | 27,000,000 | **8%** | **0** |
| test12 | 2,719,406 | 3,630,452 | 37,867,500 | **10%** | **0** |
| test7 | 2,174,960 | 2,176,200 | 2,176,200 | 100% | **0** |

region 比组内容 bbox 大 0.4%（testpdf-ai）到 33%（test12）：前者是 AA 外扩与 ceil 的必然
余量，后者来自"多个不相交绘制的包围盒并集 + clip 与 AA 余量"——仍然是 10 倍的收窄，
且 region 偏大只会多花时间，不会出错。**leak 全页为 0**。

**顺带**：`mask_render_ns` 在 test12 上 63.8 → 55.1 ms —— 遮蔽组自身的子渲染里也有混合组，
它们一并被收窄，这一项不在原预测内。


