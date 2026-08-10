# AI 使用政策 / AI Policy

> 本文件为中英双语。中文版在前，英文版在后（见下文 `English version`）。
> 两个版本具有同等效力；如有歧义，以中文版为准。
>
> This document is bilingual. The Chinese version follows first; the English
> version is below under `English version`. Both are equally authoritative; in
> case of discrepancy, the Chinese version prevails.

---

# AI 使用政策（中文）

zpdf 允许在贡献中使用 AI（大语言模型 / 代码助手）作为工具。但你对自己发布的代码负责，
维护者也对合入并发布的代码负责。我们对所有贡献保持同样的质量标准，无论是人写的还是
AI 辅助完成的。

一句话概括本政策：**你可以用 AI，但你必须理解 AI 帮你做了什么。**

## 核心原则：理解而非照搬

AI 生成的代码、测试、文档或提交信息可以出现在贡献中，前提是你**理解其中的每一行**，
并能在评审时用自己的话解释。具体而言：

- 你能说清这段代码解决什么问题、为什么这样写、有哪些边界情况与取舍。
- 你读过 AI 产出的每一行，确认它符合本仓库的约定（见 [CONTRIBUTING.md](CONTRIBUTING.md)）：
  错误用 `Result` 处理、遵守 `ParseLimits`、恶意/畸形输入不 panic、无未受控的资源消耗、
  公共 API 兼容性、引用 ISO 32000 相关章节等。
- 你能回答维护者针对该改动的追问——包括 PDF 规范依据、内存/安全影响、为何不用其他写法。

把 AI 当成打字快的协作者，而不是把决策外包给它。**不理解的内容不要提交**：先读懂、再改对、
再提交。如果某段 AI 输出你无法解释，要么删掉重写，要么把它当成阅读材料去补齐理解，而不是
直接合入。

## 提交信息与人类说明

`commit` 和 `PR` 的正文可以包含 AI 辅助生成的内容，但**必须在最下方用你自己的、人类的语言
简要说明你真正想做什么**。这段人类说明是政策的核心，不是可选项。

**这段人类说明用你的母语撰写。** 用你最习惯的语言，才能最真实地反映你对改动的理解与
判断；中文或英文皆可，重点是「你自己的话」而非措辞。

格式建议（附在 commit body / PR description 末尾）：

```text
## Human note
<In your own words, in your mother tongue (Chinese or English): what this change
is trying to do, why you took this approach, and what you verified. One or two
points is enough — keep it short, but it must reflect your own understanding and
judgment, not an AI summary.>
```

要求：

- **用母语**写这段说明，并用自己的话，而不是把 AI 的总结复制过来。我们想看的是你的意图与判断。
- 至少说清「I want to do X because Y」。如果改动涉及取舍或风险，也一并点明。
- 即便整段代码由 AI 生成，这段说明也必须由你亲自写。它是你“理解了这次改动”的凭证。
- 提交信息标题仍遵循 `type(scope): summary` 约定（见 [CONTRIBUTING.md](CONTRIBUTING.md)）；
AI 说明放在正文末尾即可，不影响标题。

## 与维护者的沟通

- **不要用 AI 撰写你在 issue / PR 中的评论与回复。** 我们希望看到人写的讨论。
  维护者可能隐藏那些明显由 AI 生成的评论。
- 开 issue 时，请用你自己的话描述问题——复现步骤、预期与实际、你的环境。
- 回答维护者提问时，请基于你自己的理解作答。**不要把 AI 的回答直接贴上来**；
如果你需要引用 AI 的一段内容，必须放在引用块（`>`）里并明确标注来源，同时附上你自己的
解说：这段为何相关、意味着什么、你怎么看。不要贴大段原文。
- 非英语母语贡献者可以用 AI 润色语言或翻译——但请先确认它表达的是你自己的观点与声音。
若用于翻译，建议用你的母语写出原意，再把 AI 译文附在引用块中。

## 自主智能体

- **不允许使用自主智能体（autonomous agents）自动开启 PR 或 issue。** 我们会关闭任何被认为
是自动创建的 PR。
- AI 可以辅助你写代码、跑测试、查资料，但「决定提什么、提交、回答评审」这些环节必须由人来完成。

## 不允许用 AI 代写的场景

- 带有 `good first issue` / `E-easy` / `E-has-instructions` 标签的 issue：这类问题本意是让
新人借此熟悉代码库，请**不要用 AI 直接代写代码**。你可以用 AI 帮助你**理解**代码库与思路，
但代码要自己写。
- 涉及安全敏感区域（解压、xref、加密、签名、路径处理、导出写文件、WebAssembly / GPU 隔离边界
等）的改动：可以借助 AI，但你必须额外仔细核对边界条件、资源限制与失败路径，并在 PR 中明确说明
你如何验证了这些点。参见 [SECURITY.md](SECURITY.md)。

## AI 辅助时的额外检查

AI 生成的内容常常在以下方面出错，请在提交前重点核对：

- `Result` 与 `?` 的错误传播是否完整，有没有被悄悄吞掉或换成 `unwrap` / `expect`。
- `ParseLimits`（递归深度、流大小、图像像素、操作符数等）是否正确传入与尊重。
- 对畸形/对抗性输入是否优雅失败，而非 panic 或无限消耗。
- 公共 API 是否保持兼容；如有破坏性改动，是否在 PR 中显式标注。
- 是否引入了 C/C++ 依赖——本仓库坚持纯 Rust（见 [CLAUDE.md](CLAUDE.md)）。
- 是否包含受版权或机密来源的 PDF 夹具；优先使用最小化的合成样本。

## 归因与披露

- 我们请你**披露** AI 工具的使用：在 commit / PR 的人类说明里，简要写明哪些部分借助了 AI、
用在了哪里（如 "Tests were AI-assisted; I reviewed and adjusted each one"）。披露是建立信任的方式，
不是把 AI 用法藏起来。
- 披露只需简短、真实，无需长篇。重点是配合上面的「人类说明」，让人能判断你对这次改动的
掌握程度。

---

# English version

zpdf allows the use of AI (large language models / coding assistants) as a tool
for contributing. You remain responsible for the code you publish, and we remain
responsible for any code we merge and release. We hold the same quality bar for
all contributions, whether written by humans or produced with AI assistance.

This policy in one line: **You may use AI, but you must understand what the AI
did for you.**

## Core principle: understand, don't relay

AI-generated code, tests, documentation, or commit messages may appear in a
contribution, provided you **understand every line** and can explain it in your
own words during review. Concretely:

- You can state what problem the code solves, why it is written this way, and
  what the edge cases and trade-offs are.
- You have read every line of the AI output and confirmed it follows this
  repository's conventions (see [CONTRIBUTING.md](CONTRIBUTING.md)): errors go
  through `Result`, `ParseLimits` is respected, malformed/adversarial input
  does not panic, there is no uncontrolled resource consumption, public API
  compatibility is preserved, relevant ISO 32000 sections are cited, and so on.
- You can answer maintainers' follow-up questions on the change — including the
  PDF-specification basis, memory/security impact, and why other approaches were
  not taken.

Treat AI as a fast-typing collaborator, not as something to outsource decisions
to. **Do not submit anything you do not understand**: read it, fix it, then
submit it. If you cannot explain a piece of AI output, either delete and rewrite
it, or treat it as reading material to build your own understanding — do not
merge it in directly.

## Commit messages and the human note

The body of a `commit` or `PR` may contain AI-assisted content, but **you must
add a short note in your own human words at the bottom stating what you were
genuinely trying to do**. This human note is the heart of the policy, not
optional.

**This human note is written in your mother tongue.** Use the language you think
in — Chinese or English — so the note genuinely reflects your understanding and
judgment; what matters is that it is in your own words, not the wording.

Suggested format (append to the end of the commit body / PR description):

```text
## Human note
<In your own words, in your mother tongue (Chinese or English): what this change
is trying to do, why you took this approach, and what you verified. One or two
points is enough — keep it short, but it must reflect your own understanding and
judgment, not an AI summary.>
```

Requirements:

- **Write this note in mother tongue**, in your own words — not by copying an AI
  summary. We want to see your intent and judgment.
- At minimum, state "I want to do X because Y." If the change involves trade-offs
  or risk, note those too.
- Even if the entire code block was generated by AI, you must write this note
  yourself. It is your proof that you understand the change.
- The commit title still follows the `type(scope): summary` convention (see
  [CONTRIBUTING.md](CONTRIBUTING.md)); the AI note goes in the body and does not
  affect the title.

## Communicating with maintainers

- **Do not use AI to write your comments or replies in issues / PRs.** We expect
  human-written discussion. Maintainers may hide comments that appear to be
  AI-generated.
- When opening an issue, describe the problem in your own words — reproduction
  steps, expected vs. actual, your environment.
- When answering maintainers' questions, answer from your own understanding.
  **Do not paste AI responses directly.** If you need to quote a passage from an
  AI, put it in a blockquote (`>`) and label it as such, then add your own
  commentary on why it is relevant, what it means, and what you think. Do not
  paste long excerpts.
- Non-native English speakers may use AI to polish language or translate — but
  take the time to ensure it reflects your own voice and ideas. For
  translation, we suggest writing the original in your native language and
  including the AI translation in a blockquote.

## Autonomous agents

- **Autonomous agents are not allowed to open PRs or issues automatically.** We
  will close any pull request we believe was created autonomously.
- AI may help you write code, run tests, or research — but the steps of deciding
  what to submit, submitting it, and answering review must be done by a human.

## Cases where AI must not author code

- Issues labeled `good first issue` / `E-easy` / `E-has-instructions`: these are
  meant to help newcomers learn the codebase. **Do not let AI write the code for
  you.** You may use AI to help you *understand* the codebase and the approach,
  but write the code yourself.
- Changes touching security-sensitive areas (decompression, xref, encryption,
  signatures, path handling, file writes during export, WebAssembly / GPU
  isolation boundaries, etc.): AI may help, but you must carefully verify
  boundary conditions, resource limits, and failure paths, and state in the PR
  how you verified them. See [SECURITY.md](SECURITY.md).

## Extra checks when AI is involved

AI-generated content frequently goes wrong in these areas — check them carefully
before submitting:

- Is `Result` / `?` error propagation complete, or has it been silently dropped
  or replaced with `unwrap` / `expect`?
- Is `ParseLimits` (recursion depth, stream size, image pixels, operator count,
  etc.) threaded in and respected?
- Does malformed / adversarial input fail gracefully rather than panic or
  consume resources without bound?
- Is the public API kept compatible; and if there is a breaking change, is it
  called out explicitly in the PR?
- Does it introduce a C/C++ dependency? This repository stays pure Rust (see
  [CLAUDE.md](CLAUDE.md)).
- Does it include PDF fixtures from copyrighted or confidential sources? Prefer
  minimal, synthetic samples.

## Attribution and disclosure

- We ask you to **disclose** your use of AI tools: in the human note of your
  commit / PR, briefly state which parts were AI-assisted and how (e.g. "Tests
  were AI-assisted; I reviewed and adjusted each one"). Disclosure builds trust;
  it is not about hiding your use of AI.
- Keep it short and truthful — no need for length. The point, together with the
  human note above, is to let others judge how well you understand the change.

---

本政策参考了 [rust-analyzer 的 AI 政策](https://github.com/rust-lang/rust-analyzer/blob/master/AI_POLICY.md)，
后者改编自 [uv 的 AI 政策](https://github.com/astral-sh/.github/blob/c5187e200db51bfe11d56e13053d29bd3793fdd8/AI_POLICY.md)。

This policy was adapted from [rust-analyzer's AI policy], which was adapted
from [uv's AI policy].

[uv's AI policy]: https://github.com/astral-sh/.github/blob/c5187e200db51bfe11d56e13053d29bd3793fdd8/AI_POLICY.md
