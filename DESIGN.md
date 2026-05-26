# zpdf — 设计文档

> 纯 Rust PDF 解析与 wgpu GPU 渲染库

## 1. 项目定位

zpdf 是一个从零构建的纯 Rust PDF 解析库，同时提供基于 wgpu 的 GPU 加速渲染后端。
目标是填补 Rust 生态中"PDF 解析 + GPU 渲染"完整方案的空白。

**设计原则：**
- 零 C/C++ 依赖，全链路纯 Rust
- 解析层与渲染层严格分离，通过 DisplayList 中间表示解耦
- 安全优先：PDF 是不可信输入格式，所有解析路径设防御性限制
- Lazy 解析：xref 记录字节范围，按需解析并缓存对象

## 2. 架构总览

### 2.1 Crate 拓扑

```text
zpdf (workspace root)
├── crates/
│   ├── zpdf-core/            基础类型与错误定义
│   ├── zpdf-parser/          PDF 文件解析器
│   ├── zpdf-document/        文档对象模型
│   ├── zpdf-content/         内容流解释器
│   ├── zpdf-display-list/    渲染中间表示
│   ├── zpdf-font/            字体引擎
│   ├── zpdf-image/           图像解码
│   ├── zpdf-color/           颜色空间与管理
│   ├── zpdf-render/          渲染后端 trait
│   ├── zpdf-render-cpu/      CPU 参考渲染 (tiny-skia)
│   ├── zpdf-render-wgpu/     wgpu GPU 渲染后端
│   ├── zpdf-cli/             命令行工具
│   └── zpdf/                 门面 crate，聚合 re-export
├── examples/
├── tests/
│   ├── corpus/               测试 PDF 文件
│   └── render_diffs/         渲染对比结果
└── fuzz/                     模糊测试
```

### 2.2 依赖流向（严格单向）

```text
zpdf-core
    │
    ├── zpdf-parser ──► zpdf-document ──► zpdf-content ──► zpdf-display-list
    │                       ▲                  ▲                   │
    ├── zpdf-font ──────────┘                  │                   │
    ├── zpdf-image ────────────────────────────┘                   │
    ├── zpdf-color ────────────────────────────┘                   │
    │                                                              │
    └── zpdf-render (trait) ◄──────────────────────────────────────┘
            │
            ├── zpdf-render-cpu
            └── zpdf-render-wgpu
```

**关键约束：** `zpdf-render-wgpu` 和 `zpdf-render-cpu` 只依赖 `zpdf-display-list` 和 `zpdf-render`，
绝不依赖 `zpdf-parser`。这保证渲染后端可独立替换。

## 3. 各 Crate 职责

### zpdf-core
基础类型，被所有其他 crate 依赖。

| 类型 | 说明 |
|------|------|
| `ObjectId(u32, u16)` | PDF 间接对象标识（对象号 + 世代号）|
| `PdfObject` | 枚举：Null/Bool/Integer/Real/String/Name/Array/Dict/Stream/Ref |
| `PdfString` | 字面量字符串与十六进制字符串 |
| `PdfName` | PDF Name 对象（`/Type` → `"Type"`）|
| `PdfDict` | 有序键值对字典 |
| `PdfStream` | 字典 + 原始字节范围（lazy decode）|
| `Matrix` | 3x3 仿射变换矩阵（PDF 使用 6 元素表示）|
| `Rect` | 矩形（x0, y0, x1, y1）|
| `Point` | 二维坐标 |
| `ParseLimits` | 安全限制配置 |
| `Error` / `Result` | 统一错误类型 |

```rust
pub struct ParseLimits {
    pub max_object_depth: u32,      // 防递归炸弹，默认 100
    pub max_stream_bytes: u64,      // 单流最大字节，默认 256 MB
    pub max_image_pixels: u64,      // 单图最大像素，默认 100M
    pub max_page_operators: u64,    // 单页最大操作符，默认 1M
    pub max_string_length: u32,     // 字符串最大长度，默认 64 KB
}
```

### zpdf-parser
低层文件解析，将字节流转为 PdfObject。

- **Header 解析**: `%PDF-X.Y` 版本检测
- **Lexer**: tokenize PDF 语法（数字、字符串、Name、关键词）
- **Object 解析**: 直接对象与间接对象（`12 0 obj ... endobj`）
- **Xref 解析**: 传统 xref table + PDF 1.5+ xref stream
- **Trailer 解析**: trailer 字典、`/Prev` 增量更新链
- **Object Stream**: 从压缩对象流中提取对象
- **Stream Filter Pipeline**: 按 `/Filter` 和 `/DecodeParms` 链式解码

支持的 Filters（按 Phase 递进）：

| Filter | Phase | 依赖 |
|--------|-------|------|
| FlateDecode | P1 | flate2 |
| ASCIIHexDecode | P1 | 手写 |
| ASCII85Decode | P1 | 手写 |
| RunLengthDecode | P1 | 手写 |
| LZWDecode | P2 | weezl |
| DCTDecode (JPEG) | P2 | zune-jpeg |
| JPXDecode (JPEG2000) | P4 | hayro-jpeg2000 |
| CCITTFaxDecode | P4 | 手写或评估 fax crate |
| JBIG2Decode | P4 | 评估中 |
| Crypt | P4 | 手写 |

### zpdf-document
文档对象模型，提供结构化访问。

```rust
pub struct PdfDocument {
    data: Arc<[u8]>,                // 原始文件数据（零拷贝引用）
    store: ObjectStore,             // 对象缓存（lazy 解析）
    xref: XrefTable,                // 对象偏移表
    trailer: TrailerChain,          // trailer 链
    catalog: Catalog,               // 文档目录
    limits: ParseLimits,
}

pub struct ObjectStore {
    cache: HashMap<ObjectId, PdfObject>,  // 已解析对象缓存
    xref: XrefTable,                       // 字节偏移索引
}

pub struct Catalog {
    pub pages: PageTree,
    pub outlines: Option<ObjectId>,
    pub names: Option<ObjectId>,
    pub metadata: Option<ObjectId>,
}

pub struct PageTree { ... }         // 递归页面树

pub struct PdfPage {
    pub id: ObjectId,
    pub media_box: Rect,
    pub crop_box: Rect,
    pub bleed_box: Option<Rect>,
    pub trim_box: Option<Rect>,
    pub art_box: Option<Rect>,
    pub rotate: i32,                // 0, 90, 180, 270
    pub resources: ResourceDict,    // 继承合并后的资源
    pub contents: Vec<ObjectId>,    // 内容流引用
}

pub struct ResourceDict {
    pub fonts: HashMap<PdfName, ObjectId>,
    pub xobjects: HashMap<PdfName, ObjectId>,
    pub color_spaces: HashMap<PdfName, ObjectId>,
    pub patterns: HashMap<PdfName, ObjectId>,
    pub shadings: HashMap<PdfName, ObjectId>,
    pub ext_g_state: HashMap<PdfName, ObjectId>,
}
```

Resources 继承：Page 未定义的资源项从 Pages 父节点逐级继承。

### zpdf-content
内容流操作符解释器，将 PDF 绘制指令转为 RenderCommand 序列。

**操作符分类：**

| 类别 | 操作符 |
|------|--------|
| 图形状态 | `q` `Q` `cm` `w` `J` `j` `M` `d` `ri` `i` `gs` |
| 路径构造 | `m` `l` `c` `v` `y` `h` `re` |
| 路径绘制 | `S` `s` `f` `F` `f*` `B` `B*` `b` `b*` `n` |
| 裁剪 | `W` `W*` |
| 文本 | `BT` `ET` `Tc` `Tw` `Tz` `TL` `Tf` `Tr` `Ts` `Td` `TD` `Tm` `T*` `Tj` `TJ` `'` `"` |
| 颜色 | `CS` `cs` `SC` `SCN` `sc` `scn` `G` `g` `RG` `rg` `K` `k` |
| XObject | `Do` |
| 标记内容 | `BMC` `BDC` `EMC` `MP` `DP` |
| 内联图像 | `BI` `ID` `EI` |

```rust
pub struct ContentInterpreter {
    state_stack: Vec<GraphicsState>,
    current: GraphicsState,
    display_list: DisplayList,
    current_path: PathBuilder,
    text_object_active: bool,
}

pub struct GraphicsState {
    pub ctm: Matrix,                    // Current Transformation Matrix
    pub stroke_color: Paint,
    pub fill_color: Paint,
    pub line_width: f32,
    pub line_cap: LineCap,              // Butt=0, Round=1, Square=2
    pub line_join: LineJoin,            // Miter=0, Round=1, Bevel=2
    pub miter_limit: f32,
    pub dash_pattern: DashPattern,
    pub rendering_intent: RenderingIntent,
    pub flatness: f32,
    pub font: Option<FontRef>,
    pub font_size: f32,
    pub text_state: TextState,
    pub stroke_alpha: f32,              // CA
    pub fill_alpha: f32,                // ca
    pub blend_mode: BlendMode,
    pub soft_mask: Option<SoftMask>,
    pub clip_depth: u32,
}

pub struct TextState {
    pub char_spacing: f32,              // Tc
    pub word_spacing: f32,              // Tw
    pub h_scaling: f32,                 // Tz (百分比)
    pub leading: f32,                   // TL
    pub rise: f32,                      // Ts
    pub render_mode: TextRenderMode,    // Tr (0-7)
    pub text_matrix: Matrix,            // Tm
    pub line_matrix: Matrix,
}
```

### zpdf-display-list
渲染中间表示，是解析层和渲染层之间的桥梁。

```rust
pub struct DisplayList {
    pub page_rect: Rect,
    pub commands: Vec<RenderCommand>,
}

pub enum RenderCommand {
    FillPath {
        path: Path,
        rule: FillRule,
        paint: Paint,
        alpha: f32,
    },
    StrokePath {
        path: Path,
        style: StrokeStyle,
        paint: Paint,
        alpha: f32,
    },
    DrawGlyphRun(GlyphRun),
    DrawImage(ImageDraw),
    PushClip {
        path: Path,
        rule: FillRule,
    },
    PopClip,
    PushBlendGroup {
        blend_mode: BlendMode,
        isolated: bool,
        knockout: bool,
        bounds: Rect,
    },
    PopBlendGroup,
    SetSoftMask(SoftMaskData),
    ClearSoftMask,
}

pub struct GlyphRun {
    pub font_id: FontId,
    pub font_size: f32,
    pub glyphs: Vec<PositionedGlyph>,
    pub paint: Paint,
    pub alpha: f32,
    pub render_mode: TextRenderMode,
}

pub struct PositionedGlyph {
    pub glyph_id: u16,
    pub position: Point,
    pub advance: f32,
}

pub struct ImageDraw {
    pub image_id: ImageId,
    pub transform: Matrix,          // 图像空间 → 页面空间
    pub alpha: f32,
    pub soft_mask: Option<ImageId>,
}

pub struct StrokeStyle {
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f32,
    pub dash: Option<DashPattern>,
}

pub enum Paint {
    Solid(Color),
    Pattern(PatternId),
    Shading(ShadingId),
}

pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

pub enum FillRule {
    NonZero,
    EvenOdd,
}

pub enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}
```

### zpdf-font
PDF 字体处理。

**职责：**
- 解析 PDF 字体字典（Type1, TrueType, CIDFont, Type0, Type3）
- 字符编码映射（Encoding, CMap, ToUnicode）
- 提取字形轮廓（glyph outline）用于渲染
- 字形度量（宽度、bounding box、ascent/descent）
- 嵌入字体数据提取与解析

**依赖链：**
- `skrifa` — OpenType/TrueType 字体二进制解析
- `rustybuzz` — 文本 shaping（连字、上下文替换）
- `swash` — 字形光栅化

```rust
pub struct FontCache {
    fonts: HashMap<FontId, LoadedFont>,
}

pub struct LoadedFont {
    pub pdf_font_type: PdfFontType,
    pub encoding: FontEncoding,
    pub to_unicode: Option<CMapTable>,
    pub font_data: Option<Arc<[u8]>>,   // 嵌入字体原始数据
    pub metrics: FontMetrics,
}

pub enum PdfFontType {
    Type1,
    TrueType,
    Type0 { descendant: CIDFontInfo },
    Type3 { char_procs: HashMap<PdfName, ObjectId> },
    MMType1,
}
```

### zpdf-image
PDF 图像处理。

**职责：**
- Image XObject 解析（宽、高、颜色空间、位深）
- 压缩数据解码（链式 filter）
- 图像掩码（ImageMask, SMask, 色键掩码）
- 内联图像（BI/ID/EI）解析
- 颜色空间转换到 RGBA

```rust
pub struct ImageCache {
    images: HashMap<ImageId, DecodedImage>,
}

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,              // RGBA 像素
    pub has_alpha: bool,
    pub premultiplied: bool,
}
```

### zpdf-color
颜色空间与颜色管理。

| 颜色空间 | Phase | 说明 |
|----------|-------|------|
| DeviceGray | P2 | 单通道灰度 |
| DeviceRGB | P2 | 标准 RGB |
| DeviceCMYK | P2 | CMYK → RGB 近似转换 |
| CalGray | P4 | CIE 灰度 |
| CalRGB | P4 | CIE RGB |
| Lab | P4 | CIE L*a*b* |
| ICCBased | P4 | ICC Profile 驱动 |
| Indexed | P4 | 调色板 |
| Separation | P4 | 专色 |
| DeviceN | P4 | 多通道分色 |
| Pattern | P4 | 图案填充 |

### zpdf-render
后端无关的渲染 trait 定义。

```rust
pub trait RenderBackend {
    type Target;
    type Error: std::error::Error;

    fn begin_page(&mut self, info: &PageRenderInfo) -> Result<(), Self::Error>;
    fn execute(&mut self, cmd: &RenderCommand) -> Result<(), Self::Error>;
    fn end_page(&mut self) -> Result<Self::Target, Self::Error>;
}

pub struct PageRenderInfo {
    pub page_rect: Rect,
    pub scale: f32,                 // DPI / 72.0
    pub background: Color,
}
```

### zpdf-render-cpu
基于 tiny-skia 的 CPU 参考渲染器。

- 输出 `Vec<u8>` (RGBA pixel buffer) 或 PNG
- 用于正确性基线测试
- 无 GPU fallback 场景

### zpdf-render-wgpu
基于 wgpu 的 GPU 加速渲染器。

**渲染管线设计：**

| Pipeline | 用途 | 顶点格式 | 混合 |
|----------|------|----------|------|
| `solid_fill` | 纯色路径填充 | pos(f32x2) + color(f32x4) | Alpha Blend |
| `textured` | 图像绘制 | pos(f32x2) + uv(f32x2) | Alpha Blend |
| `glyph` | 文字渲染 | pos(f32x2) + uv(f32x2) + color(f32x4) | Alpha Blend (R8 atlas) |
| `stencil_fill` | 裁剪路径写入 | pos(f32x2) | Stencil Only |

**关键组件：**
- `WgpuContext` — Instance/Adapter/Device/Queue 管理
- `GlyphAtlas` — R8Unorm 字形纹理图集，LRU 淘汰
- `TextureCache` — 图像纹理缓存，BindGroup 池
- `BatchBuilder` — 按 pipeline/texture/blend/clip 排序批处理
- `ClipStack` — Stencil buffer 管理裁剪嵌套

**wgpu 资源使用：**
- TextureFormat: `Rgba8Unorm` (颜色目标), `R8Unorm` (字形遮罩)
- BlendState: `PREMULTIPLIED_ALPHA_BLENDING` (默认)
- Stencil: `IncrWrap`/`DecrWrap` 管理裁剪层级
- PrimitiveTopology: `TriangleList`
- 路径 tessellation: lyon CPU tessellation → GPU draw

### zpdf-cli
命令行工具。

```text
zpdf info <file>              显示 PDF 元数据
zpdf dump <file> <obj> <gen>  dump 指定对象
zpdf render <file> -p <page> -o <output.png> --dpi <dpi>
zpdf text <file> -p <page>    提取文本
zpdf bench <file>             渲染性能测试
```

### zpdf (门面 crate)
聚合 re-export，用户只需 `use zpdf::*;`。

```rust
// 典型使用
let doc = zpdf::Document::open("input.pdf")?;
let page = doc.page(0)?;

// CPU 渲染
let pixels = page.render_to_pixmap(300.0)?;  // 300 DPI
pixels.save_png("output.png")?;

// GPU 渲染
let mut gpu = zpdf::gpu::Renderer::new(&wgpu_device, &wgpu_queue)?;
let texture = gpu.render_page(&page, 2.0)?;  // 2x scale
```

## 4. 技术选型

### 纯 Rust 依赖

| 领域 | Crate | 版本 | 用途 |
|------|-------|------|------|
| 解析组合器 | winnow | 1.x | PDF lexer 子解析 |
| 几何 | kurbo | 0.13 | Affine/BezPath/Rect |
| GPU tessellation | lyon | latest | 路径三角化 |
| CPU 渲染 | tiny-skia | latest | 参考渲染器 |
| OTF/TTF 解析 | skrifa | latest | 字体二进制解析 |
| 文本 shaping | rustybuzz | latest | HarfBuzz Rust port |
| 字形光栅化 | swash | latest | 字形到像素 |
| Flate/zlib | flate2 | 1.x (rust_backend) | FlateDecode |
| JPEG | zune-jpeg | 0.5 | DCTDecode |
| JPEG2000 | hayro-jpeg2000 | latest | JPXDecode (评估) |
| PNG 输出 | image | 0.25 | 图像 buffer/PNG 编码 |
| ICC 颜色 | moxcms | latest | ICC profile (评估) |
| wgpu | wgpu | =29.0.0 | GPU 后端 (初始 pin) |
| 错误处理 | thiserror | 2.x | Error derive |
| 日志 | tracing | 0.1 | 结构化日志 |
| 小数组 | smallvec | 1.x | 栈上小集合 |
| 位标志 | bitflags | 2.x | 标志类型 |
| GPU 数据 | bytemuck | 1.x | 安全 transmute |

### 必须自行实现的部分

- PDF object/xref/trailer/object stream 解析器
- 内容流解释器与 graphics state 栈
- Resource 继承、Form XObject 递归、ExtGState
- PDF 字体编码、CMap、ToUnicode 映射逻辑
- Stream filter pipeline、DecodeParms、Predictor
- PDF 颜色空间语义（DeviceCMYK→RGB, Indexed 展开等）
- 透明组、soft mask、blend mode 合成
- DisplayList → wgpu 的 batching、clip、texture atlas、offscreen pass

## 5. 安全设计

PDF 是不可信输入格式，解析器必须防御：

| 威胁 | 防御 |
|------|------|
| 递归炸弹（嵌套对象/数组） | `max_object_depth` 限制 |
| 解压炸弹（tiny stream → huge data） | `max_stream_bytes` 限制 |
| 图像炸弹（极大分辨率） | `max_image_pixels` 限制 |
| 无限循环引用 | 对象引用访问集合检测 |
| 畸形 xref | 尾部扫描 fallback + 偏移验证 |
| 无限操作符流 | `max_page_operators` 限制 |

## 6. 测试策略

| 层级 | 方法 |
|------|------|
| 单元测试 | 每个 crate 内部 `#[cfg(test)]` |
| 集成测试 | `tests/corpus/` 下真实 PDF 文件解析验证 |
| 渲染对比 | 与 MuPDF/Poppler 输出做像素容差 diff |
| 模糊测试 | `cargo-fuzz` 针对 lexer/object parser/content stream |
| 性能基准 | `criterion` 解析与渲染基准 |
