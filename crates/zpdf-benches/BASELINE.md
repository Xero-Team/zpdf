# zpdf 基准套件

> 实测结果与候选排序见 `docs/performance/PERFORMANCE.md` §13。本文只讲**怎么跑**、
> **数字的可信边界**，以及**为什么这么设计**。

## 为什么重做

上一轮的基准有三个结构性缺陷，测量本身因此不可信：

1. **语料静默缺失**：`tests/` 整个被 gitignore，仓库里 0 个 PDF。缺失文件只打一行
   stderr 然后跳过、**退出码 0** —— 全新 clone 上 `cargo bench` 测了个空却报成功。
2. **标签手写且错**：标为 "image-heavy" 的是 346 字形的文本页，标为 "text-heavy" 的是
   0 字形的图像页。基于这些标签的结论全部继承了错误。
3. **单点 DPI + 临时探针**：只有 150 DPI 一个点（无法区分填充率绑定与几何绑定），
   阶段探针是跑完即删的 env-var 代码，无法进 CI。

本套件针对这三点重建。

## 四个 target

```bash
cargo bench -p zpdf-benches --bench stages     # 五阶段分解
cargo bench -p zpdf-benches --bench backend    # CPU 冷/暖 + CLI 进程 + GPU 双数
cargo bench -p zpdf-benches --bench batch      # 多页吞吐 + 1/2/4/8/16 线程 scaling
cargo bench -p zpdf-benches --features bench-long --bench longform   # 全量文档（默认不跑）
```

GPU 需要 feature：`--features gpu-render`。

| target | 组 | 测什么 |
|---|---|---|
| `stages` | `parse` `interpret` `render_cpu` `encode` `total` | 五阶段 × 语料 × DPI{96,150,300}；每阶段的前置阶段放在（不计时的）setup |
| `backend` | `cpu_fresh` `cpu_reused` `cpu_stats` `cli_process` `gpu_readback` `gpu_submit_only` | 见下文"两种冷" |
| `batch` | `sequential` `parallel` | 页级并行 ceiling（rayon 仅 dev-dep） |
| `longform` | — | 整本文档的绝对墙钟（526 页级），criterion 不适合，故为普通 main |

### 两种"冷"不可混称

- **渲染器级**：`cpu_fresh`（每次迭代新建 `CpuRenderer`）vs `cpu_reused`。实测差异在噪声内
  （test8：9.89 vs 9.84 ms）——**渲染器生命周期不是冷启动成本的来源**。
- **进程级**：`cli_process` 直接 spawn release CLI。同页 **91.3 ms**，是进程内渲染的 9.3×。
  差距来自进程启动 + parse + interpret + encode + PNG 写盘。

只说"冷渲染慢 20×"而不指明是哪一种，就不是测量。`cli_process` **只接受 release 二进制**：
debug CLI 单页约 15 s，混在同一张表里会被误读为可比。

### GPU 数字必须带适配器身份

本机除 RTX 5080 外还有 **2 个虚拟显示适配器**（远程桌面镜像、Android 模拟器），
`request_adapter` 会静默选中其中之一。因此任何 GPU 计时都必须连同
`AdapterIdentity`（名称/后端/设备类型/驱动/MSAA/timestamp 支持）一起记录；
`baseline --compare` 会比对指纹并在不一致时**明确警告**（不硬失败：换机是合法的）。

- `gpu_readback` = CLI 消费模式（含 readback 与阻塞 poll）
- `gpu_submit_only` = viewer 消费模式（**无 readback**）
- 两者**双数并报**；用 readback 的 wall 代表"GPU 渲染成本"会高估约 3×（实测 8.65 → 2.67 ms）
- 开 `with_gpu_timing` 会**多一个同步点**，所以 headline 数字取**关计时**的运行

## 语料

| 语料 | 位置 | 入 git | 校验 |
|---|---|---|---|
| 合成（9 个 / 22 KB） | `tests/corpus/` + `tests/gen_corpus.py` | ✅ | git 本身 |
| 真实 PDF（19 个） | `tests/test*` `testpdf` `zzztest` | ❌（`/tests` 被忽略） | `crates/zpdf-benches/corpus-manifest.tsv`（sha256 + 页数 + **实测分类** + **覆盖率**） |
| `tests/failed` <1MB（497 个 / 30.6 MB） | `tests/failed/` | ✅ | git |
| `tests/failed` ≥1MB（121 个） | 同上 | ❌（含 1 个 269 MB，超 GitHub 100 MB 硬限） | `tests/failed-manifest.tsv` |

**缺失或哈希不符默认硬失败**，并打印修复指引。唯一的降级模式是显式的
`ZPDF_BENCH_SYNTHETIC_ONLY=1`，且会大声声明"数字不可与全语料基线比较"。

重新生成 / 分类：

```bash
cargo run -p zpdf-benches --bin corpus-manifest -- --update     # 重建（class/coverage 置 -）
cargo run -p zpdf-benches --bin corpus-manifest -- --classify   # 实测分类 + 覆盖率
bash tests/track_failed_subset.sh                               # 重放 failed <1MB 子集
```

**负载分类是测量出来的**，不是文件名猜的：`Composition` 把三类工作量折算成**可比的
device-px 覆盖**（图像用变换行列式、字形用 em-box 代理、矢量用路径 bbox 代理），
并按"某类 ≥ 2× 次类"判定。单页选择取**每类最重 + 路径序补足**——否则 29 M px 的
`testpdf-ai` 会因路径序第 5 而落选。

注意**覆盖率 ≠ 耗时**：`test11` 按覆盖率是 vector，却把 66% 时间花在 glyph 上。

## 样本数按实测成本选择

语料横跨 ~10 ms（文本页）到 ~400 ms（图像页）。单一采样数无法同时服务两端：
100 samples 在最慢页上要 39 s，且 criterion 在样本慢时会**自动延长**测量窗口，
把 3 s 预算静默变成 40 s。故 `common::samples_for` 按 setup 实测的单次成本取
10/30/100。目标：默认（不含 `longform`）≤ ~10 分钟。

## 门禁：能做什么，不能做什么

**能**（`crates/zpdf-benches/tests/perf_gate.rs`，CI 每次跑）：

- **确定性计数器**：合成语料每个 fixture 的 DL 组成（命令/填充/描边/字形/图像数）逐项精确断言。
  整数在任何机器上都一致，故**零 flake**，能精确抓住算法级回退（如 `pattern_tiling` 的
  961 条命令——一个平铺图案在 DL 里展成它的每一块瓦片）。
- **宽松墙钟上限**：只抓挂死与数量级爆炸，不抓 5%。

**不能**：CI 相对阈值门禁。共享 runner 的噪声足以让 5% 阈值随机失败，而一个会随机
失败的门禁会被无视——比没有门禁更糟。

**本机相对比较**：

```bash
cargo bench -p zpdf-benches --features gpu-render --bench stages --bench backend --bench batch
cargo run --release -p zpdf-benches --bin baseline -- --record     # 更新检出基线
cargo run --release -p zpdf-benches --bin baseline -- --compare    # 超 +5% 非零退出
```

`baseline/baseline.json` 记录**机器指纹**（CPU/核数/OS/arch）、**git commit**、
**语料 manifest 哈希**与 case 数——一个没有这些的基准数字无法与任何东西比较，
包括它自己的重跑。环境不一致时**报绝对数并警告**，不硬失败。

> 小注：criterion 把 id 里的 `/` 在**结果目录名**中改写为 `_`，所以 `--compare` 输出里
> 组名形如 `stages_interpret/test8-text@96`，而报告里显示 `stages/interpret/...`。
> 两种拼写都稳定，比较按同一拼写匹配，不影响判定。

## 当前基线的可信边界（务必先读）

`baseline/baseline.json`（2026-09-12，`2777fef`，149 个 case）录自一次**部分完成**的运行：

- ✅ 完整：`backend`（含 GPU 两个组与 `cli_process`）、`stages` 的 `parse`/`interpret`/多数 `render_cpu`
- ⚠️ 缺失：`stages` 的 `encode`/`total`、`batch` 全部（当次因 GPU OOM 中断，OOM 已修）

`--compare` 会**列出基线里有、本次没测的 case**，不会拿部分数据假装完整。在完整跑一遍
（`--bench stages --bench backend --bench batch`）之后重新 `--record` 即可补齐。

## 已知未测

- **页级并行 ceiling（C1）**：`batch` target 尚未完整跑出——这是当前最大的未测杠杆。
- 300 DPI 的 `encode`/`total`。
