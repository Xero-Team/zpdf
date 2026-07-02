# zpdf — 开发路线图

## Phase 1：PDF 解析基础

> 目标：能打开真实 PDF，解析全部对象，遍历页面树，但不渲染。

### P1.1 — 项目骨架
- [x] Cargo workspace 搭建
- [x] zpdf-core: 对象模型、错误类型、几何基础类型
- [x] CI 配置（GitHub Actions：fmt / clippy / test 多平台 + tag 触发 release）

### P1.2 — Lexer 与 Object 解析
- [x] PDF header 解析（`%PDF-X.Y`）
- [x] Lexer: 空白/注释跳过、数字、字符串（literal/hex）、Name、关键词
- [x] Direct object 解析: null/bool/int/real/string/name/array/dict
- [x] Indirect object 解析: `N G obj ... endobj`
- [x] Stream 边界识别（`stream\n ... endstream`）

### P1.3 — Xref 与 Trailer
- [x] 传统 xref table 解析
- [x] Trailer 字典解析
- [x] `/Prev` 增量更新链追踪
- [x] Xref stream 解析（PDF 1.5+, `/Type /XRef`）
- [x] Object stream 解析（`/Type /ObjStm`）
- [x] 尾部扫描 fallback（损坏 xref 恢复）：全文件 `N G obj` 扫描重建 xref + trailer；
      catalog 可在 `/ObjStm` 内/被同号对象遮蔽/`/Type` 被翻转时仍定位；无 catalog 也能打开；
      缺失/free 引用回退至修复表；详见下方"健壮性专项"

### P1.4 — Stream Filters
- [x] Filter pipeline 框架（链式解码）
- [x] FlateDecode（flate2）
- [x] ASCIIHexDecode
- [x] ASCII85Decode
- [x] RunLengthDecode
- [x] DecodeParms / Predictor 支持

### P1.5 — 文档对象模型
- [x] Catalog 解析
- [x] PageTree 遍历
- [x] PdfPage 构建（MediaBox/CropBox/Rotate）
- [x] ResourceDict 继承合并
- [x] zpdf-cli: `info` 和 `dump` 命令
- [x] ObjectStore: lazy 解析 + 缓存（`PdfFile::resolve` 按需解码 + `object_cache`/`objstm_cache`）

### P1.6 — 测试与安全
- [x] 真实 PDF 端到端测试（1.5MB, 16 页中文文档）
- [x] ~~手写最小 PDF 测试用例~~ 已由真实数据集覆盖（tests/failed 618 个对抗性 PDF + zpdf-parser 内联最小 PDF 单测）
- [x] ~~Object stream 测试用例~~ 同上（ObjStm 解码路径由真实语料 + `objstm_header_and_slicing_math` 单测覆盖）
- [x] ParseLimits 验证（递归深度/流大小/字符串长度/对象数上限，见 lexer/object_parser/recovery 单测）
- [x] cargo-fuzz 目标：lexer, object parser（+ filters/content_tokenizer/parse_pdf，共 5 个目标；CI: fuzz.yml 每夜运行）

### P1 里程碑验收
```
cargo run -p zpdf-cli -- info tests/corpus/minimal.pdf
# 输出: PDF-1.7, 3 pages, 45 objects
cargo run -p zpdf-cli -- dump tests/corpus/minimal.pdf 1 0
# 输出: << /Type /Catalog /Pages 2 0 R >>
```

---

## Phase 2：内容流解释与 CPU 渲染

> 目标：解释内容流，产出 DisplayList，CPU 参考渲染输出 PNG。

### P2.1 — 内容流 Tokenizer
- [x] 操作符/操作数 tokenizer
- [x] 操作数类型识别（数字、字符串、Name、数组）
- [x] inline image (BI/ID/EI) 解析（缩写键/色彩空间归一化 + 长度/EI 扫描）

### P2.2 — Graphics State 与操作符解释
- [x] GraphicsState 栈（q/Q）
- [x] 变换矩阵（cm）
- [x] 路径构造：m/l/c/v/y/h/re
- [x] 路径绘制：S/s/f/F/f*/B/B*/b/b*/n
- [x] 裁剪路径：W/W*
- [x] 线型状态：w/J/j/M/d
- [x] ExtGState (gs) — ca/CA/LW/LC/LJ/ML

### P2.3 — 颜色操作
- [x] DeviceGray/DeviceRGB/DeviceCMYK 设置
- [x] CS/cs 颜色空间切换
- [x] SC/SCN/sc/scn 颜色值设置
- [x] 快捷操作符：G/g/RG/rg/K/k
- [x] CMYK → RGB（非 ICC 路径用 Adobe DeviceCMYK→sRGB 多项式近似，US Web Coated
      SWOP，与 Acrobat/pdf.js 对齐；100% K 为深近黑而非纯黑）

### P2.4 — 文本渲染
- [x] BT/ET 文本块
- [x] Tf 字体设置（含 FontCache 查找）
- [x] Td/TD/Tm/T* 文本定位
- [x] Tj/TJ/' /" 文本输出
- [x] Tc/Tw/Tz/TL/Ts/Tr 文本状态
- [x] Type3 字体（CharProcs 内容流字形）
- [x] TrueType 嵌入字体（ttf-parser 字形轮廓）
- [x] Type1 嵌入字体（PostScript FontFile：eexec/charstring 解释器 + subrs/flex/seac）
- [x] Type0/CID 字体（Identity-H, FontFile2）
- [x] CID /W 宽度数组解析
- [x] 基础字体：Type1 标准 14 字体
- [x] Simple Encoding (Standard/WinAnsi/MacRoman/PDFDoc/Symbol/ZapfDingbats + /Differences)
- [x] ToUnicode CMap 解析（bfchar/bfrange，UTF-16BE 代理对）

### P2.5 — 图像渲染
- [x] Image XObject 解析 (Do)
- [x] FlateDecode 图像
- [x] DCTDecode (JPEG) 图像
- [x] Image Mask / SMask
- [x] 颜色空间 → RGBA 转换
- [x] Form XObject 递归解释

### P2.6 — Display List 与 CPU 渲染
- [x] ContentInterpreter → DisplayList 完整管线
- [x] zpdf-render trait 定义
- [x] zpdf-render-cpu: tiny-skia 实现
  - [x] 路径 fill/stroke
  - [x] 文字渲染（Type3 字形轮廓 + TrueType 轮廓 → tiny-skia path）
  - [x] 图像绘制
  - [x] 裁剪（stencil）
- [x] PNG 输出
- [x] zpdf-cli: `render` 命令

### P2.7 — 文本提取
- [x] 基于 ToUnicode + Encoding 的文本提取
- [x] 字符坐标与排序（按行 Y 分组 + 行内 X 排序，自适应行距）
- [x] zpdf-cli: `text` 命令（`-p <page>` / `--all`）

### P2 里程碑验收
```
cargo run -p zpdf-cli -- render tests/corpus/sample.pdf -p 1 -o output.png --dpi 150
# 输出可辨识的 PDF 页面 PNG
cargo run -p zpdf-cli -- text tests/corpus/sample.pdf -p 1
# 输出页面文本
```

---

## Phase 3：wgpu GPU 渲染后端

> 目标：用 GPU 渲染 Phase 2 的 DisplayList，达到交互帧率。

### P3.1 — wgpu 上下文
- [x] WgpuContext: Instance/Adapter/Device/Queue（headless，pollster 阻塞）
- [ ] Surface 配置（窗口模式）— 留待 viewer (M9)
- [x] Offscreen 渲染（headless）— PageTarget + copy_texture_to_buffer 回读
- [x] MSAA 支持（4x/1x 协商，含 Stencil8 同采样数）

### P3.2 — 渲染管线
- [x] solid_fill pipeline（纯色路径，premultiplied blend + D5 stencil 测试）
- [ ] textured pipeline（图像）— M7
- [x] glyph 渲染：矢量填充基线（轮廓 + Type3 走 solid_fill，精确 outline_to_pixel）+ R8Unorm 图集快速路径（M6b，轴对齐字形，见 P3.4）
- [x] stencil_fill pipeline（裁剪：clip_write + clip_reset）
- [x] WGSL shader 编写（solid.wgsl：pixel→NDC + premultiplied 实色；其余 shader 待后续里程碑）

### P3.3 — 路径渲染
- [x] lyon tessellation 集成（BezPath → TriangleList，device-pixel 空间）
- [x] Fill: non-zero / even-odd
- [x] Stroke: 线宽/端点/接合（虚线刻意忽略，与 CPU oracle 对齐）
- [x] 顶点缓冲区管理（per-page 合并 vertex/index buffer，Immediate 模式）

### P3.4 — 文字渲染
- [x] 矢量填充基线（M6a）：轮廓字形按 CPU 精确坐标 lyon 三角化 → solid_fill；Type3 经 ContentInterpreter 子 DisplayList 渲染。3 个真实 PDF compare 0.34–0.58%，Type3 合成用例 0.000%
- [x] GlyphAtlas: R8Unorm 纹理图集 — M6b（per-page，仅轴对齐/非镜像字形；tiny-skia 光栅化，与 CPU oracle 同 AA 算法，非 atlas-able 时优雅回退矢量填充）
- [x] LRU 淘汰策略 — M6b（shelf packing + 图集满时淘汰单个最近最少使用槽位并复用其矩形）
- [x] 字形 quad 生成（位置 + UV）— M6b（纯平移布局：raster 内 pen 原点对齐 device-pixel 目标位置）
- [x] 批量绘制 — M6b（同 clip_ref 的相邻 glyph quad 经 P3.7 batch_ops 合并为单个 draw_indexed）
> 关键教训：图集缓存键必须用毫像素（millipixel，1/1000 px）精度桶化字号——整像素取整（≤0.5px 误差）在常见 9–15px 正文字号下是显著的相对形变，真实 PDF 实测曾令 GPU-vs-CPU 差异从基线 ~1.3% 骤增至 ~3.9%；改为毫像素精度后合成语料回到 0.000%（`text_outline_repeated`），单元测试新增 `fractional_millipixel_size_is_not_snapped_to_whole_pixels` 防回归。真实文本密集页仍有 ~2.5pp 残留差异，经 heatmap 定位严格限于字形边缘（图集纹理双线性重采样在任意亚像素定位下的固有 AA 近似，非结构性缺陷）——在正式验收语料（<1% 门槛）之外，不阻塞发布。

### P3.5 — 图像渲染
- [x] 图像上传（Rgba8Unorm，write_texture）+ per-image BindGroup（按 image_id 缓存）
- [x] 变换矩阵（render_image 仿射两分支烘焙进 quad 顶点，含 ctm_flips_y）
- [x] 带 alpha 的图像混合（texel 视为 premultiplied 匹配 tiny-skia + 逐 draw opacity；裁剪 stencil 测试）
> 真实图像页 compare 0.015%；image_rgb（3 图含 Y 翻转）0.559%；image_under_clip 0.481%

### P3.6 — 裁剪与混合
- [x] ClipStack: stencil buffer 管理（Stencil8，有序 op 列表 replay）
- [x] 嵌套 clip 层级（clip_write IncrementClamp 累积交集 + PopClip 全屏 reset 重建）
- [x] Alpha blending (Normal blend mode)（premultiplied source-over）
- [x] Premultiplied alpha 管线
- [x] Blend group 离屏合成（M8：RenderLayer 栈 + scratch composite，多 pass）/ 16 种 blend mode（composite.wgsl，W3C premultiplied 公式）
> 注：内容解释器尚未发出 PushBlendGroup（CPU/GPU 后端均已实现该 op）；M8 经程序化 DisplayList 验证：Multiply 叠加 = 黑（精确），6 种模式与 CPU oracle 一致 <1%

### P3.7 — 批处理优化
- [x] BatchBuilder（batch.rs）：单趟线性扫描，合并严格相邻、同 key（pipeline+clip_ref；图像/字形另加 image_id）的 draw op 为一个 draw_indexed——不重排，仅合并已连续的序列，保持 alpha blending 的绘制顺序依赖安全
- [x] PageRecorder::append 在写入时烘焙绝对索引（`base_vertex` 恒为 0），使同 arena 内连续同 key 绘制天然可拼接，无需重放期 base_vertex 记账
- [x] Uniform buffer 复用（page uniform 单 buffer；pipeline 切换最小化）

### P3.8 — CLI 后端 + 示例 Viewer
- [x] CLI `--backend [cpu|wgpu]`（M3，hand-rolled parser + 显式校验 + save_rgba 共享）
- [x] winit 0.30 窗口 + wgpu surface（examples/viewer.rs，winit 仅 dev-dependency）
- [x] 缩放/平移/翻页（滚轮/+/- 缩放，WASD/方向键，PageUp/Down 翻页）
- [x] 渲染缓存（page tile：每页渲染一次 → blit；翻页才重栅格化）
- [x] GPU timing 统计 — timing.rs：`wgpu::Features::TIMESTAMP_QUERY`（adapter 不支持时优雅降级，`ts_supported: bool`）；`WgpuRenderer::last_gpu_time_ns()`；CLI `render --stats` 打印 CPU 墙钟 + （wgpu 后端且支持时）GPU pass 耗时
- [x] CI 验收 harness：crates/zpdf/tests/gpu_acceptance.rs（gpu-render gated，10 合成用例 GPU vs CPU <1% + markup_annotations 专项测试，无 adapter 时优雅跳过；M6b 新增 `text_outline_repeated`（重复字形覆盖图集路径）与 `glyph_atlas_path_is_genuinely_exercised`（分数字号确认图集非回退矢量填充）两个用例）

### P3 里程碑验收 — ✅ 全部达成（M1–M9 + 性能项）
> GPU 后端渲染填充/描边/曲线/裁剪/文本/图像/混合组，均与 CPU oracle 对齐。
> 合成语料 + 真实 PDF 单页均 <1%；真实 16→62 页中文文档 52/62 页 <1%（其余 1.0–1.4%，
> 为致密 CJK 的 analytic-vs-MSAA AA 差异，R1 已知限制，threshold≈24–32 下全部通过）。
> P3.4 GlyphAtlas、P3.7 批处理、P3.8 GPU timing 均已实现（原延后的性能项，详见各节）；
> blend group op 解释器尚未发出（后端已就绪）。
```
cargo run -p zpdf-cli --features gpu -- render <file.pdf> -p 1 -o gpu.png --backend wgpu
cargo run -p zpdf-cli --features gpu -- render <file.pdf> -p 1 -o cpu.png --backend cpu
cargo run -p zpdf-cli -- compare cpu.png gpu.png        # <1% 差异
cargo run -p zpdf-cli --features gpu -- render <file.pdf> -p 1 -o gpu.png --backend wgpu --stats   # CPU/GPU 耗时
cargo test -p zpdf --features gpu-render                # 验收 harness
cargo run -p zpdf-render-wgpu --example viewer -- <file.pdf>   # 交互浏览器
```
> 像素对比工具已就绪：`zpdf compare <a.png> <b.png> [--out diff.png] [--threshold N]`
> 输出 差异像素% / MAE / RMSE / 最大通道差，并生成差异热力图（GPU↔CPU 验收可直接复用）。

---

## Phase 4：高级功能

> 目标：覆盖 PDF 1.7 / 2.0 常见复杂文件。
>
> **0.3.0 兼容性专项**（见 docs/CHANGELOG.md）完成了本阶段大部分条目，并补强了
> Phase 1/2 范围外的健壮性：混合 xref（/XRefStm）、xref 偏移修复、损坏 Flate 流
> 抢救、悬空引用按规范解析为 null、CropBox 渲染、/Rotate //Resources 页面树继承、
> 虚线、细线 hairline、双线性图像采样、文本渲染模式（OCR 隐藏文本）等。
> 56 页真实文档对照 pdfium 逐页验证。
>
> **损坏/对抗性语料健壮性专项**（见 docs/CHANGELOG.md 0.5.0）：对 618 个
> 畸形/对抗性 PDF（tests/failed，来自 PDFBOX/Ghostscript/poppler/MOZILLA/PDFIUM/
> cairo 等的 bug 与 fuzzer）做了一轮加固。可打开文档 166 → 426；渲染 panic 13 → 0；
> 渲染超时 110 → 0、打开期挂起 2 → 0。手段：宽松 `%PDF` 头 + 无头碎片对象扫描恢复、
> 全缓冲 `startxref` 搜索、catalog（含 `/ObjStm` 内/遮蔽/翻转 `/Type`）恢复与无 catalog
> 仍打开、`resolve` 对缺失/free 项查修复表、文档级 `/Type /Page`/页面形状扫描兜底、
> 默认 MediaBox、宽松 dict 解析；渲染侧防崩溃/防挂起：路径有限性边界检查（消除 tiny-skia
> panic）、`hayro-jpeg2000` `catch_unwind`、64 Mpx 栅格上限、按页 clip 像素预算 +
> bbox 限定裁剪、解释器命令/操作符预算、解释/渲染两阶段挂钟兜底。剩余 192 个无法打开者
> 为真正不可恢复（口令加密、<400B 截断碎片、非 PDF、页面对象被 fuzzer 删除）。

### P4.1 — 完整字体支持
- [x] CIDFont (Type0 composite fonts)：Identity-H、`/W`、`/CIDToGIDMap` 流、
      OpenType 包装的 CID-keyed CFF
- [x] CMap 解析（预定义 + 嵌入）：嵌入 CMap 流 + Identity / UniGB/UniCNS/UniJIS/UniKS
      UCS2/UTF16 系列；legacy 字节编码 CMap 全覆盖（GB2312/GBK、Big5、Shift-JIS（含半角
      片假名）、EUC-KR/UHC、EUC-JP，均含 -H/-V）——非嵌入 CJK 字体经 `LegacyEncoding`
      解码 code→Unicode 走替代字体；码表由 `tools/gen_cjk_tables.py` 烘焙（Python 编解码器，无新依赖）
- [x] Vertical writing mode（WMode 1：按 /DW2 推进 + 字形原点居中）
- [x] Type3 font (字形由内容流定义)：含间接 /CharProcs //Encoding //Widths，
      FirstChar≠0 修复
- [x] Font fallback (缺失字体替代)：扫描系统字体目录，按 PostScript 名 / 全名 /
      家族+样式 + CID 排序默认匹配（`zpdf_font::system`）
- [x] Variable fonts（OpenType `fvar`/`gvar`/CFF2）：FontDescriptor 选择子驱动变体轴
      （`/FontWeight`→`wght`、`/FontStretch`→`wdth`、`/ItalicAngle`→`slnt`、Italic 标志→`ital`），
      经 `ttf-parser` `set_variation`（按轴范围钳制 + `avar`）在取轮廓/度量前应用；静态字体为
      no-op（缺失轴被忽略），`/Widths` 仍主导定位；Type0 经后代 CIDFont 找描述符

### P4.2 — 完整颜色管理
- [x] ICCBased 颜色空间：经 moxcms 通过嵌入 profile 转换（矢量/shading/调色板/图像
      路径）；无可用 profile 时按 /N 回退设备空间
- [x] CalGray / CalRGB / Lab（Lab→XYZ→sRGB 解析转换；CalGray/CalRGB 近似设备空间）
- [x] Indexed 颜色空间（填充 + 图像调色板，含 Indexed-over-Lab）
- [x] Separation / DeviceN（经 PDF 函数评估器走 tint transform → alternate）
- [x] PDF 函数评估器（type 0/2/3/4，zpdf-color/src/function.rs）
- [x] Overprint（PDF 8.6.7）：ExtGState `/OP`(描边) `/op`(填充，缺省随 `/OP`) `/OPM`
      解析进图形状态；按源颜色空间投影出 CMYK 着色剂与"激活通道"掩码（DeviceCMYK
      受 OPM 控制 0=knockout/1=nonzero；DeviceGray→K；Separation/DeviceN 经 tint
      变换投影到 CMYK 取非零；DeviceRGB/Lab/ICC 非 4 通道为 no-op）。`Overprint{cmyk,active}`
      随 FillPath/StrokePath/GlyphRun 进显示列表，**后端按朴素减色 CMYK 合成**：仅激活
      通道取源值、其余通道保留背景（`zpdf_core::{rgb,cmyk}_to_cmyk/rgb_naive` 互逆 →
      未触及通道精确往返）。CPU 走 scratch-render + 逐像素合并（oracle），wgpu 经离屏层 +
      composite.wgsl 新 overprint 模式合成，GPU↔CPU 6 例全 0.000%。仅作用于填充/描边/文本，
      图像/阴影 overprint 暂未覆盖
- [x] Rendering Intent（`ri` 算子 + ExtGState `/RI` + 图像 `/Intent` → moxcms 渲染意图；
      `IccCache` 按 (ObjectId, intent) 键控，含 ICC 规定回退序）

### P4.3 — 透明度与混合
- [x] 全部 16 种 blend mode（GPU composite.wgsl 实现，W3C 公式）— M8
- [x] 解释器发出 /BM → PushBlendGroup（双后端生效）
- [x] Soft Mask (luminosity/alpha)：ExtGState /SMask（含 /TR //BC）
- [x] Transparency Group：离屏合成已实现；knockout（`/K true`，逐元素 shape pass +
      `knockout_merge`，PDF 11.4.9）与 non-isolated（`/I false` 透传到 backdrop）已实现；
      仅"非隔离 + 常量 alpha/软掩码/非 Normal 混合"仍近似为 isolated
- [x] Offscreen render pass 合成（M8 RenderLayer + scratch swap）

### P4.4 — Pattern 与 Shading
- [x] Tiling Pattern (colored/uncolored)：真正 cell 平铺（form-XObject 机制，pattern
      空间锚定 base CTM·/Matrix，平铺数上限保护）
- [x] Axial Shading (Type 2)：`sh` 算子 + PatternType 2 填充（栅格化为图像）
- [x] Radial Shading (Type 3)：同上，含 /Extend 与谱系根选择
- [x] Free-form Gouraud (Type 4) + Lattice-form Gouraud (Type 5)（流位流解码，
      逐顶点字节对齐，边标志条带；Gouraud 重心插值，经图像管线双后端共用）
- [x] Coons/Tensor Patch (Type 6/7)（12/16 控制点，f=1/2/3 共享边复用表；
      Coons `S=SC+SD−SB` / 张量双三次曲面；细分三角化 + 逐顶点 RGB 解析后插值）

### P4.5 — 注释与表单
- [x] 页面 /Annots 解析 + /AP appearance stream 渲染（12.5.5 BBox→Rect 映射，
      /AS 状态，Hidden/NoView 标志）
- [x] Markup & 几何注释外观合成（无 /AP 时，`zpdf-document/src/annot_appearance.rs`）：
      文本标记 Highlight（/Multiply 混合，over 文字保留深色）/Underline/StrikeOut/Squiggly
      （按 /QuadPoints 真实四边形定向：质心角排序取凸序 + 长边定基线 + up 向量，
      旋转/倾斜文本沿基线绘制而非轴对齐包围盒）、几何 Square/Circle（/RD 内缩 +
      /BS//Border 边框 + /IC 填充）/Line（/L）/Polygon//PolyLine（/Vertices）/Ink
      （/InkList）、保守 Link 边框（仅当显式 /C + 显式非零边框，避免给每个超链接画框）。
      /C 1/3/4 分量 → 设备灰/RGB/CMYK；空 /C[] 按规范透明（不绘制）；/CA → ExtGState ca//CA。
      经既有 `GeneratedAppearance`→合成 form XObject 路径渲染，**双后端零改动**（GPU↔CPU 0.198%）。
      对抗性几何加固：字节上限（1 MiB）+ Squiggly 跨 quad 段预算 + 坐标钳制 ±1e7 +
      反相 inset 守护（618 失败语料 0 panic/0 timeout，OK 426 不变）
- [x] Line/PolyLine /LE 端点样式（Table 176：OpenArrow/ClosedArrow/R 反向变体/Butt/
      Slash/Square/Circle/Diamond/None）：沿线方向定向，按边宽缩放，闭合箭头用 /IC 填充
      （无则空心描边）；端点几何与定向四边形共用 norm/sub 矢量助手
- [x] FreeText 外观合成（12.5.6.6，`free_text`）：/Contents 按 /DA 字体/字号/颜色 + /Q
      对齐换行（复用 `forms` 文本排版引擎），可选 /C 背景、可选边框、可选 /CL 标注线
      （带 /LE 箭头）；正文经 `q … cm … BT … ET Q` 平移进框内局部坐标并裁剪；/Contents
      限长 50k 字符防对抗
- [x] Text/Stamp 标准图标外观（无 /AP 时合成，`annot_appearance.rs`）：Text 注记按
      /Name 画矢量图标（Note 折角便签 / Comment / Help 问号圈 / Insert 插入符 / Key 钥匙 /
      Check 对勾 / Cross 叉，未知名回退便签），/C 着色、居中正方图标框；Stamp 印章按
      /Name 解码标签（NotApproved→"NOT APPROVED"，默认 Draft）画圆角边框 + Helvetica-Bold
      居中标签，颜色按名约定（肯定绿/中性蓝/警示红，/C 可覆盖），/CA 透明度。经既有
      /AP 合成路径双后端渲染，纯 Rust 零新依赖；标签仅 [A-Z0-9 ]（无需转义）+ 1 MiB 上限
- [x] Widget annotation (form fields)：Widget 经字段模型映射回所属字段；checkbox/radio
      保留 /AP 状态（/AS 缺失时回退 /V）
- [x] AcroForm 字段解析（`zpdf-document/src/forms.rs`）：递归 /Fields + /Kids，完整
      限定名（/T 以 `.` 连接），继承 /FT //V //DA //Ff //Q；解析 /Opt //MaxLen；
      `PdfDocument::acro_form()` + `zpdf forms` CLI 命令
- [x] Appearance regeneration：文本/选择字段缺 /AP（或 /NeedAppearances）时合成外观流
      （/DA 字体/字号/颜色，size 0 自适应；/Q 对齐；multiline / comb / list-box 布局；
      /DR 字体解析，回退合成 Helvetica），经现有 /AP 路径双后端渲染。Button/口令/push
      不生成；既有 /AP 不被覆盖

### P4.6 — 加密
- [x] Standard Security Handler（含直接 /Encrypt 字典、/StmF //StrF /Identity）
- [x] RC4 / AES-128 / AES-256 解密（V1-V5，R2-R6；空用户口令；pypdf 加密
      fixtures 端到端验证）
- [x] 权限检查（/P 仅用于密钥派生）—— /P 仅参与密钥派生，不做强制限制（符合查看器惯例）
- [x] 口令 API（非空用户/所有者口令）：`PdfDocument::open_with_password` /
      `PdfFile::parse_with_password` / `is_encrypted()`；RC4 所有者口令经
      Algorithm 7 从 /O 恢复用户口令；错误口令返回 `Error::WrongPassword`；
      CLI `--password`。空口令默认打开仍宽松（不回归损坏语料）

### P4.7 — 附加 Filters
- [x] LZWDecode
- [x] CCITTFaxDecode (Group 3/4)
- [x] JBIG2Decode（手写 T.88 解码器）
- [x] JPXDecode (JPEG2000)（hayro-jpeg2000；畸形码流 catch_unwind 防崩溃）
- [x] Crypt filter（/StmF //StrF /Identity）

### P4.8 — 文本提取增强
- [x] 结构化文本提取（坐标、大小：TextSpan）
- [x] 阅读顺序启发式（递归 XY-cut：列检测 + 整行重组）
- [x] 表格检测（`zpdf-content/src/tables.rs`，基于对齐的白空隙网格法）：按基线聚行 →
      按大间距切带 → 带内用扫描线找"白空隙"竖列分隔（列分隔须在带内绝大多数行保持空白，
      max_cross=floor((1−0.85)·行数)）→ 列分隔中点定列、行基线中点定行 → 按 x 中心分桶建格。
      标题/说明单列行从首尾裁剪；散文因填满行宽而跨越列隙自动被排除（2 列另设填充率
      ≤0.80 守卫）。`detect_tables(&[TextSpan]) -> Vec<Table>`（cells/col_x/row_y +
      to_csv/to_tsv/bbox），CLI `zpdf tables`（`-p`//--all//--csv）。纯文本对齐法，
      无后端/渲染改动；规则线（绘制的横竖线）感知留作后续增强

### P4.9 — PDF 2.0
- [x] Output Intents（ISO 32000-1 §14.11.5 + ISO 32000-2 页面级）：解析文档级
      （catalog `/OutputIntents`）与 **PDF 2.0 页面级**（页字典 `/OutputIntents`，
      页面级覆盖文档级）；`OutputIntent` 元数据经 `PdfDocument::output_intents()` /
      `page_output_intents()` 暴露，`zpdf info` 列出 `/S`/条件标识/`/DestOutputProfile`
      通道数。当激活意图的 `/DestOutputProfile` 是 4 通道（CMYK）ICC profile 时，
      **DeviceCMYK 经该 profile 色彩管理**（PDF/X 特征化印刷条件模型）而非 Adobe SWOP
      多项式：矢量填充/描边/文本（`k`/`K`/`scn`/`sc` 与 `components_to_rgb` 单一闸口
      `cmyk_to_display`）与栅格图像（`Cmyk`→`Icc{4}`，Indexed/CMYK 调色板烘焙为 RGB）
      统一走既有 `IccTransform`/`IccCache`，复用渲染意图机制。严格门控：无可用 4 通道
      输出意图的文档逐字节不变（仍走 SWOP）；嵌入 ICCBased(4) 自带 profile 优先；
      RGB(N≠3..)/解析失败的意图被忽略并回退。后端零改动（转换在解释器上游完成，
      CPU↔GPU 自动一致）。纯 Rust 零新依赖
- [x] 嵌入文件与关联文件（`zpdf-document/src/embedded_files.rs`）：文档级嵌入文件
      （catalog `/Names /EmbeddedFiles` 名称树，ISO 32000-1 §7.11）与 **PDF 2.0
      关联文件**（catalog/页面 `/AF` 数组，ISO 32000-2 §7.11.4，携带 `/AFRelationship`）
      经统一 `EmbeddedFile` 模型暴露：文件名（`/UF` 优先，回退 `/F`/平台名）、`/Desc`、
      `/AFRelationship`、嵌入流的 `/Subtype`(MIME) 与 `/Params`(`/Size`//CreationDate/
      `/ModDate`//CheckSum)、嵌入流对象号。名称树游走带深度/逐引用环/条目数上限，处理
      `/Kids` 内部节点与 `/Names` 叶节点；元数据只读流字典，列举时不解码负载。
      `PdfDocument::{embedded_files,associated_files,page_associated_files,
      embedded_file_bytes}`（取字节经过滤管线按需解码，遵守 ParseLimits），facade
      re-export `EmbeddedFile`//EmbeddedSource。CLI `zpdf attachments`
      （`--extract <index|name|all>` / `--out-dir`，按嵌入流去重并合并 `/AF` 关系），
      **抽取时净化文件名**（`../../etc/passwd`→basename，分隔符/Windows 保留字符与设备名/
      尾随点空格中和，原子 create-new 不覆盖既有文件，冲突加 ` (n)` 后缀）杜绝越出
      `--out-dir` 或覆盖目录内文件；`zpdf info` 亦列出附件。解析路径仅
      显式调用时运行（open//render 期间不触发），畸形语料健壮性零回归。纯 Rust 零新运行时依赖
      （ZUGFeRD/Factur-X 发票 XML 抽取可用）
- [x] NChannel 颜色空间与 `None`/`All` 着色剂语义（ISO 32000-1 §8.6.6.4/§8.6.6.5、
      ISO 32000-2 NChannel）：`ActiveColorSpace::Tint` 携带着色剂名 + NChannel `/Colorants`
      逐着色剂映射。Separation `/None`（或全 `/None` DeviceN/NChannel）**不产生任何标记**
      —— 填充/描边/字形 **及 `/ImageMask` 蒙版**（用当前填充色绘制）均被抑制，文本提取不受影响；
      Separation `/All` 指代全部着色剂（套准标记）→ 非选择性叠印（正常 knockout）；NChannel
      `/Colorants`（或含 `/None` 需排除时）按**逐输入着色剂**计算叠印激活掩码：`/None` 不贡献
      墨、过程名 Cyan/Magenta/Yellow/Black 直接置位、spot 经各自 Separation 投影、无法分类的
      spot 经整体 tint 变换隔离（仍丢弃 `/None`），取并集。显示颜色不变（仍走整体 tint 变换）；
      抑制与掩码均在解释器（显示列表上游）决定，**双后端零改动**、CPU↔GPU 自动一致。无 `/Colorants`
      且无 `/None` 的普通 Separation/DeviceN 逐字节保持原整体变换投影（per-colorant 仅在可能不同时启用）
- [x] 新增注释类型（无 `/AP` 时合成外观，`annot_appearance.rs`）：**Caret**（§12.5.6.11，
      `/RD` 内缩矩形内的填充插入符 "‸"，`/C` 着色，空 `/C[]` 透明）、**Redact**（§12.5.6.23，
      标记待密文区域：`/QuadPoints` 四边形或回退 `/Rect`，`/IC` 填充 + `/C` 描边，描边按半边宽
      斜接内缩使其不被 `/Rect`=`/BBox` 裁剪；仅渲染"标记"态、不移除内容，故 `/OverlayText`//RO
      后密文叠加不绘制）。PDF 2.0 **Projection** 已识别但无既定默认外观（仅经自带 `/AP` 渲染）。
      既有 `/AP` 不被覆盖，双后端零改动

### P4.10 — 文档导航与元数据

- [x] 文档大纲 / 书签（ISO 32000-1 §12.3.3，`zpdf-document/src/outline.rs`）：解析 catalog
      `/Outlines` 树为嵌套 `OutlineItem`（`title`/已解析 `dest`/`uri`/`open`/`children`）。沿
      `/First`//Next` 游走，带深度上限 + 逐引用 visited 集（大纲根预置）+ 全局条目上限，畸形/环形
      树终止不挂起；`/Count` 符号（经解析型数值取值，indirect//Real `/Count` 亦生效）定 `open`
- [x] 目标地（destinations，§12.3.2，`destinations.rs`）：共享解析器把任意 dest——显式
      `[page /Fit …]` 数组、命名目标、`<< /D … >>` 字典、或其间接引用——解析为
      `Destination{page, page_ref, view}`。目标页引用经新增 `Catalog::page_index_of` 映射为
      0 基页号；裸页号（远程 go-to）按文档页数钳制，`page` 决不越界。八种视图模式
      （`DestView`：`/XYZ`//Fit`//FitH`//FitV`//FitR`//FitB`//FitBH`//FitBV`）全解析，
      `null` 坐标与 `/XYZ` 零缩放归一为"保留当前值"
- [x] 命名目标双注册表解析：现代 `/Names /Dests` 名称树（深度 + visited + 节点预算 + `/Limits`
      剪枝，且决不漏掉在场键）**与** 旧式 `/Root /Dests` 字典（老旧产出器仍用）；name→name
      间接按深度限界，自指命名不成环
- [x] 大纲目标解析：逐项 `/Dest` 或 `/A` 动作——go-to（`/S /GoTo`→目标地）/URI
      （`/S /URI`→超链接）/远程 go-to（`/S /GoToR`→目标文件名）
- [x] 文档信息字典（`/Info`，§14.3.3，`doc_info.rs`）：trailer `/Info` →
      `DocInfo{title,author,subject,keywords,creator,producer,creation_date,mod_date,trapped}`；
      文本串 UTF-16BE/PDFDoc 解码，日期按原始 PDF 日期串报告（不解析）；无 `/Info` 或无任何
      字段时返回 `None`
- [x] 页面标签（`/PageLabels`，§12.4.2，`page_labels.rs`）：catalog `/PageLabels`
      **数字树**解析为按起始页索引排序的标注区间。每个标签字典（Table 159）完整支持：`/S`
      编号样式（`/D` 十进制、`/R`/`/r` 大/小写罗马、`/A`/`/a` 大/小写字母 `A…Z,AA…ZZ,AAA…`）、
      `/P` 前缀、`/St` 起始值（默认 1，钳制 `≥1`）；无 `/S` 为仅前缀（无数字部分）。
      `PageLabels::label(page_index)` 取覆盖区间按 `St+(page_index−区间起点)` 计算页码，
      首区间之前的页无标签（`None`）。数字树一次性扁平化（深度/逐引用 visited/节点+条目预算），
      前缀长度上限，超大 `/St` 回退十进制（防罗马/字母串膨胀）。`PdfDocument::page_labels()`，
      facade re-export `PageLabels`/`PageLabelStyle`；`zpdf info` 在各页行附打 `label: <L>`。
      纯数据模型、零新依赖、无解析/后端改动，仅显式调用时运行（畸形语料健壮性零回归）。
      经真实 400 页加密文档端到端验证（间接数字树 → 间接标签字典，解密后报告 `1…400`）
- [x] 链接注释目标提取（§12.5.6.5，`destinations.rs`//annotation.rs`）：`Annotation` 新增
      `dest:Option<Destination>` 与 `uri:Option<String>`，由 `/Dest` 或 `/A` 动作解析——`/Dest`/
      go-to(`/GoTo`)→目标地，URI(`/URI`)→超链接，远程 go-to(`/GoToR /F`)→目标文件名。共享
      `resolve_link_target` 取代 outline 私有副本（书签与链接解析一致）；命名目标注册表按页一次性
      扁平化（`collect_named_dests`，无命名目标时廉价短路）防 O(links×tree) DoS。CLI `zpdf links`
      逐页列出链接矩形与目标（`-> p.<N>` / `-> uri:<…>`，页数上限防挂起）
- [x] XMP 元数据（`/Metadata`，§14.3.2，`xmp.rs`）：catalog `/Metadata` XMP 包（PDF 2.0 优先于
      `/Info`）经**有界字节抓取**（非 XML 引擎）读出常见 Dublin Core/XMP/PDF 模式属性 →
      `XmpMetadata{title,creators,description,subjects,keywords,producer,creator_tool,
      create_date,modify_date}`。**绝不解析任何 DTD 通用实体**（仅 5 个预定义实体 + 数字字符引用，
      各映射单字符）→ 杜绝"billion laughs"实体膨胀炸弹；扫描线性、字段/数组长度上限、包 BOM 感知
      （UTF-8/UTF-16）上限 8 MiB、对抗性多字节输入有字符边界守卫。处理简单元素/`rdf:Alt`（取
      `x-default`）/`rdf:Seq`//rdf:Bag`/RDF 属性简写。`PdfDocument::{xmp_metadata,metadata_bytes}`，
      facade re-export `XmpMetadata`；`zpdf info` 增打 `XMP Metadata:` 块。权衡：非标准命名空间前缀
      （非 `dc`//xmp`//pdf`）不识别（实践中通用）
- [x] API 与 CLI：`PdfDocument::{outline, named_destination, resolve_destination, info}`，
      facade re-export `Destination`//DestView`//OutlineItem`//DocInfo`；CLI `zpdf outline`
      缩进打印书签树（行尾 `-> p.<N>` 页 / `-> uri:<…>` 链接），`zpdf info` 增打 `Metadata:`
      块与 `Outline:` 摘要。纯数据模型、零新依赖、无解析/后端改动；仅显式调用时运行
      （open/render 期间不触发），畸形语料健壮性零回归

### P4.11 — 逻辑结构与标记 PDF（Tagged PDF）

- [x] 逻辑结构树（`/StructTreeRoot`，ISO 32000-1 §14.7 + §14.8，`structure.rs`）：把 catalog
      `/StructTreeRoot` 读为可导航的 `StructTree`——描述文档逻辑组织（标题/段落/列表/表格/插图）
      的**结构元素**树，独立于页面版式。这是屏幕阅读器所遍历、也是语义化提取（而非"一袋字形"）
      所依赖的层。结构元素（`/Type /StructElem`）→ 嵌套 `StructElem`，携带角色（`/S`）、原始类型、
      标题（`/T`）、语言（`/Lang`）、无障碍文本（`/Alt`//ActualText`）、缩写展开（`/E`）、有效页与子项
- [x] 角色解析与分类：`/S` 经结构树根 `/RoleMap`**传递式**解析（带名字环上限），分类为 `StructRole`
      枚举的标准结构类型——分组（`Document`//Part`//Sect`//Div`//TOC` …）、块级（`P`//H1`…`H6`、
      `L`//LI`//Lbl`//LBody`、`Table`//TR`//TH`//TD`//THead`//TBody`//TFoot`）、行内（`Span`//Quote`//Note`//Link`//BibEntry`、
      `Ruby`//Warichu` …）、插图（`Figure`//Formula`//Form`）。未映射到标准类型者为 `StructRole::Other(name)`，
      原始 `/S` 始终保留于 `raw_type`
- [x] 子项（`/K`，单值或数组）归一为 `StructKid`：嵌套元素、**标记内容**序列（裸整数 MCID 或
      `/Type /MCR` 字典，携带其索引的内容流页号）、或**对象引用**（`/Type /OBJR`，如参与结构的注释）。
      `/Pg` 继承：元素有效页取自身 `/Pg` 或最近祖先（经 catalog 页反查表映射为 0 基页号），并传递给
      MCID/OBJR 子项
- [x] 无障碍辅助：`StructElem::accessible_text()`（`/ActualText` 优先于 `/Alt`）、
      `StructRole::{as_str, is_standard, is_heading}`、`StructTree::element_count()`；标记态
      `PdfDocument::is_tagged()`（catalog `/MarkInfo /Marked true`，与是否有 `/StructTreeRoot` 独立）
- [x] 健壮性：仅触及对象图、仅显式调用时运行（open/render 期间不触发）。全程有界——深度上限、
      **以结构树根引用预置**的逐引用 visited 集（子项回指根不产生伪元素）、按节点*及*逐 `/K` 数组项
      examined 扣减的共享预算、`/RoleMap` 解析深度上限 + 名字环守卫、`/RoleMap` 条目数上限、
      `/Alt`//ActualText`//T`//E`//Lang` 逐串长度上限。畸形语料契约验证（环形子项/根回指/角色映射环/
      超深链/数 MiB `/Alt` 均干净终止），失败语料零回归
- [x] API 与 CLI：`PdfDocument::{struct_tree, is_tagged}`，facade re-export `StructTree`/`StructElem`/
      `StructKid`/`StructRole`；CLI `zpdf struct` 缩进打印结构树（角色 + 可选标题/无障碍文本 + 页，
      MCID/OBJR 子项作 `·` 叶），`zpdf info` 增打 `Tagged PDF`//Structure tree` 摘要。纯数据模型、
      零新依赖、无解析/后端改动；真实标记文档端到端验证（16 页标记 PDF 报出完整
      `Document → H1/P/Figure/…` 树，MCID 与页关联均解析）
- [x] 标记 PDF 阅读顺序文本提取（MCID → 内容绑定）：内容解释器**为每个抽取文本串捕获其
      标记内容 id（`/MCID`）**——与既有 `BMC`//BDC`//EMC` 深度并行维护一个 MCID 栈，从每个
      `BDC` 属性操作数（内联字典或经 `/Properties` 解析的名字）读 `/MCID`，`TextSpan` 新增
      `mcid:Option<i32>` 取**最内层包围序列**的 id（无 id 的嵌套序列继承外层，§14.6//14.7.4.2），
      畸形 id（负/非整数）丢为 `None`。MCID 栈在**每个内容流边界保存/恢复**——form XObject 与
      注释外观流取新作用域（form 标记内容不渗入页面、form 内未闭合 `BDC` 不外泄），平铺图案
      格逐格重置，页重置清空；Type3 字形过程由后端渲染期解释、抽取期不触及，故无需处理。
      `zpdf_content::text::struct_ordered_text(spans, page_index, tree)` 按文档序游走结构树：
      每元素贡献其 `/ActualText`（元素及子项的精确替换）、或其 `/MCID` 子项所引页面内容的文本、
      或（无内容元素如插图）其 `/Alt`；块级角色（`StructRole::is_block_level`）换行分隔，块内按
      与 `spans_to_text` 一致的词距/换行启发式拼接；匹配**按页**（MCID 为逐内容流），未标记/无
      标记内容的页**回退几何 `spans_to_text`**。**结构性双后端安全**：MCID 仅随 `TextSpan`，
      后端消费的 `DisplayList`//RenderCommand` 逐字节不变（测试断言装/不装文本汇时渲染命令一致），
      故 CPU↔GPU 像素一致性不可能回归，且捕获仅在装文本汇（抽取）时运行、open/render 期不触发。
      facade re-export `struct_ordered_text`；CLI `zpdf text --struct`。纯 Rust 零新依赖、无解析/
      后端改动；真实标记文档端到端验证（页面上离行放置的内联代码段——几何提取会错位——被还原到
      句子的正确阅读位置）

---

## 时间估算（参考）

| Phase | 预计工作量 | 累计 |
|-------|-----------|------|
| P1 | 3-4 周 | 3-4 周 |
| P2 | 6-8 周 | 9-12 周 |
| P3 | 4-6 周 | 13-18 周 |
| P4 | 持续迭代 | — |

P1+P2 完成后即可发布 `0.1.0`（CPU 渲染可用）。
P3 完成后发布 `0.2.0`（GPU 渲染可用）。
P4 按子功能逐步发布小版本。
