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
- [ ] ObjectStore: lazy 解析 + 缓存

### P1.6 — 测试与安全
- [x] 真实 PDF 端到端测试（1.5MB, 16 页中文文档）
- [ ] 手写最小 PDF 测试用例
- [ ] Object stream 测试用例
- [ ] ParseLimits 验证（递归深度/流大小）
- [ ] cargo-fuzz 目标：lexer, object parser

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
- [x] glyph 渲染：矢量填充基线（轮廓 + Type3 走 solid_fill，精确 outline_to_pixel）；R8 atlas 优化留待 M6b
- [x] stencil_fill pipeline（裁剪：clip_write + clip_reset）
- [x] WGSL shader 编写（solid.wgsl：pixel→NDC + premultiplied 实色；其余 shader 待后续里程碑）

### P3.3 — 路径渲染
- [x] lyon tessellation 集成（BezPath → TriangleList，device-pixel 空间）
- [x] Fill: non-zero / even-odd
- [x] Stroke: 线宽/端点/接合（虚线刻意忽略，与 CPU oracle 对齐）
- [x] 顶点缓冲区管理（per-page 合并 vertex/index buffer，Immediate 模式）

### P3.4 — 文字渲染
- [x] 矢量填充基线（M6a）：轮廓字形按 CPU 精确坐标 lyon 三角化 → solid_fill；Type3 经 ContentInterpreter 子 DisplayList 渲染。3 个真实 PDF compare 0.34–0.58%，Type3 合成用例 0.000%
- [ ] GlyphAtlas: R8Unorm 纹理图集 — M6b（可选优化，仅轴对齐字形）
- [ ] LRU 淘汰策略 — M6b
- [ ] 字形 quad 生成（位置 + UV）— M6b
- [ ] 批量绘制 — M6b（当前走共享 arena / Immediate）

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

### P3.7 — 批处理优化（延后：可选性能项）
- [~] 当前为 Immediate 模式（每命令一个 draw_indexed，共享 per-page arena buffer），正确且对现有负载足够快
- [ ] BatchBuilder: 按 pipeline/texture/clip 排序合并 — 延后，仅在吞吐成为瓶颈时实现
- [x] Uniform buffer 复用（page uniform 单 buffer；pipeline 切换最小化）

### P3.8 — CLI 后端 + 示例 Viewer
- [x] CLI `--backend [cpu|wgpu]`（M3，hand-rolled parser + 显式校验 + save_rgba 共享）
- [x] winit 0.30 窗口 + wgpu surface（examples/viewer.rs，winit 仅 dev-dependency）
- [x] 缩放/平移/翻页（滚轮/+/- 缩放，WASD/方向键，PageUp/Down 翻页）
- [x] 渲染缓存（page tile：每页渲染一次 → blit；翻页才重栅格化）
- [ ] GPU timing 统计 — 未实现（性能遥测，延后）
- [x] CI 验收 harness：crates/zpdf/tests/gpu_acceptance.rs（gpu-render gated，7 合成用例 GPU vs CPU <1%，无 adapter 时优雅跳过）

### P3 里程碑验收 — ✅ 基本达成（M1–M9）
> GPU 后端渲染填充/描边/曲线/裁剪/文本/图像/混合组，均与 CPU oracle 对齐。
> 合成语料 + 真实 PDF 单页均 <1%；真实 16→62 页中文文档 52/62 页 <1%（其余 1.0–1.4%，
> 为致密 CJK 的 analytic-vs-MSAA AA 差异，R1 已知限制，threshold≈24–32 下全部通过）。
> 批处理（P3.7）与 GPU timing 延后；blend group op 解释器尚未发出（后端已就绪）。
```
cargo run -p zpdf-cli --features gpu -- render <file.pdf> -p 1 -o gpu.png --backend wgpu
cargo run -p zpdf-cli --features gpu -- render <file.pdf> -p 1 -o cpu.png --backend cpu
cargo run -p zpdf-cli -- compare cpu.png gpu.png        # <1% 差异
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
> **损坏/对抗性语料健壮性专项**（见 docs/CHANGELOG.md "Unreleased"）：对 618 个
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
- [ ] Variable fonts

### P4.2 — 完整颜色管理
- [x] ICCBased 颜色空间：经 moxcms 通过嵌入 profile 转换（矢量/shading/调色板/图像
      路径）；无可用 profile 时按 /N 回退设备空间
- [x] CalGray / CalRGB / Lab（Lab→XYZ→sRGB 解析转换；CalGray/CalRGB 近似设备空间）
- [x] Indexed 颜色空间（填充 + 图像调色板，含 Indexed-over-Lab）
- [x] Separation / DeviceN（经 PDF 函数评估器走 tint transform → alternate）
- [x] PDF 函数评估器（type 0/2/3/4，zpdf-color/src/function.rs）
- [ ] Overprint
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
- [ ] Link / Text / Highlight annotation
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
- [ ] 表格检测

### P4.9 — PDF 2.0
- [ ] ISO 32000-2 差异项
- [ ] 新增颜色空间
- [ ] 新增注释类型

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
