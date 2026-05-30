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
- [ ] 尾部扫描 fallback（损坏 xref 恢复）

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
- [x] CMYK → RGB 简单转换

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
- [ ] WgpuContext: Instance/Adapter/Device/Queue
- [ ] Surface 配置（窗口模式）
- [ ] Offscreen 渲染（headless）
- [ ] MSAA 支持

### P3.2 — 渲染管线
- [ ] solid_fill pipeline（纯色路径）
- [ ] textured pipeline（图像）
- [ ] glyph pipeline（文字，R8 atlas 采样）
- [ ] stencil_fill pipeline（裁剪）
- [ ] WGSL shader 编写

### P3.3 — 路径渲染
- [ ] lyon tessellation 集成（BezPath → TriangleList）
- [ ] Fill: non-zero / even-odd
- [ ] Stroke: 线宽/端点/接合/虚线
- [ ] 顶点缓冲区管理

### P3.4 — 文字渲染
- [ ] GlyphAtlas: R8Unorm 纹理图集
- [ ] LRU 淘汰策略
- [ ] 字形 quad 生成（位置 + UV）
- [ ] 批量绘制

### P3.5 — 图像渲染
- [ ] TextureCache: 图像上传 + BindGroup 缓存
- [ ] 变换矩阵 → uniform buffer
- [ ] 带 alpha 的图像混合

### P3.6 — 裁剪与混合
- [ ] ClipStack: stencil buffer 管理
- [ ] IncrWrap/DecrWrap clip 层级
- [ ] Alpha blending (Normal blend mode)
- [ ] Premultiplied alpha 管线

### P3.7 — 批处理优化
- [ ] BatchBuilder: 按 pipeline/texture/clip 排序
- [ ] 合并连续同类 draw call
- [ ] Uniform buffer 复用

### P3.8 — 示例 Viewer
- [ ] winit 窗口 + wgpu surface
- [ ] 缩放/平移/翻页
- [ ] 渲染缓存（page tile）
- [ ] GPU timing 统计

### P3 里程碑验收
```
cargo run -p zpdf-cli -- render tests/corpus/sample.pdf -p 1 -o gpu_output.png --backend wgpu
# GPU 渲染输出 PNG，与 CPU 渲染结果 < 1% 像素差异
cargo run --example viewer -- tests/corpus/sample.pdf
# 交互式 PDF 浏览器，60fps 缩放/翻页
```
> 像素对比工具已就绪：`zpdf compare <a.png> <b.png> [--out diff.png] [--threshold N]`
> 输出 差异像素% / MAE / RMSE / 最大通道差，并生成差异热力图（GPU↔CPU 验收可直接复用）。

---

## Phase 4：高级功能

> 目标：覆盖 PDF 1.7 / 2.0 常见复杂文件。

### P4.1 — 完整字体支持
- [ ] CIDFont (Type0 composite fonts)
- [ ] CMap 解析（预定义 + 嵌入）
- [ ] Vertical writing mode
- [ ] Type3 font (字形由内容流定义)
- [ ] Font fallback (缺失字体替代)
- [ ] Variable fonts

### P4.2 — 完整颜色管理
- [ ] ICCBased 颜色空间（moxcms 集成）
- [ ] CalGray / CalRGB / Lab
- [ ] Indexed 颜色空间
- [ ] Separation / DeviceN
- [ ] Overprint
- [ ] Rendering Intent

### P4.3 — 透明度与混合
- [ ] 全部 16 种 blend mode（GPU shader 实现）
- [ ] Soft Mask (luminosity/alpha)
- [ ] Transparency Group (isolated/knockout)
- [ ] Offscreen render pass 合成

### P4.4 — Pattern 与 Shading
- [ ] Tiling Pattern (colored/uncolored)
- [ ] Axial Shading (Type 2)
- [ ] Radial Shading (Type 3)
- [ ] Free-form Gouraud (Type 4)
- [ ] Coons/Tensor Patch (Type 6/7)

### P4.5 — 注释与表单
- [ ] Annotation appearance stream
- [ ] Link / Text / Highlight annotation
- [ ] Widget annotation (form fields)
- [ ] AcroForm 字段解析
- [ ] Appearance regeneration

### P4.6 — 加密
- [ ] Standard Security Handler
- [ ] RC4 / AES-128 / AES-256 解密
- [ ] 权限检查

### P4.7 — 附加 Filters
- [ ] LZWDecode
- [ ] CCITTFaxDecode (Group 3/4)
- [ ] JBIG2Decode
- [ ] JPXDecode (JPEG2000)
- [ ] Crypt filter

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
