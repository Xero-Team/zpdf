# Contributing to zpdf

Thank you for helping improve zpdf. Contributions of focused bug fixes, tests,
documentation, performance work, and new PDF features are welcome.

## Before you start

- Search open and closed issues before opening a new one.
- Use the structured issue forms and keep one problem or proposal per issue.
- Open an issue before investing in a large feature, public API change, new
  dependency, or architectural rewrite so the direction can be agreed on.
- Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
  Never include exploit details, credentials, or confidential PDFs in an issue.
- If you use AI tools (LLMs / coding assistants) when contributing, follow
  [AI_POLICY.md](AI_POLICY.md). In short: you may use AI, but you must
  understand what it did for you, and add a short human-written note in your own
  words (your mother tongue) at the bottom of each commit/PR explaining what you
  intended to do.
- Only submit PDF fixtures and other assets that you have permission to
  redistribute. Prefer reduced or synthetic samples.

For focused fixes, tests, and documentation corrections, you can open a pull
request directly.

## Development setup

zpdf is a Rust workspace. Install the current stable Rust toolchain and the
formatting and linting components:

```bash
rustup toolchain install stable --component rustfmt --component clippy
git clone https://github.com/Xero-Team/zpdf.git
cd zpdf
cargo build --workspace
```

The GPUI viewer requires native windowing and font libraries. On Ubuntu, the CI
runner installs them with:

```bash
sudo apt-get update
sudo apt-get install -y \
  libfontconfig-dev \
  libwayland-dev \
  libx11-xcb-dev \
  libxkbcommon-x11-dev
```

The repository also provides an optional pre-commit hook that mirrors the
formatting and Clippy gates:

```bash
git config core.hooksPath .githooks
```

## Workspace map

The main review areas are:

- `zpdf-core`, `zpdf-parser`: primitive types, object parsing, filters, and
  untrusted-input boundaries.
- `zpdf-document`, `zpdf-content`, `zpdf-display-list`: document semantics,
  operators, extraction, and renderable display lists.
- `zpdf-font`, `zpdf-image`, `zpdf-color`: resource decoding and color/font
  handling.
- `zpdf-render`, `zpdf-render-cpu`, `zpdf-render-wgpu`: shared, CPU, and GPU
  rendering paths.
- `zpdf-writer`: creation, editing, encryption, signatures, optimization, and
  serialization.
- `zpdf-cli`, `zpdf-viewer-gpui`, `zpdf-wasm`: user-facing integrations.
- `zpdf-svg-export`, `zpdf-pptx-export`: export backends.

See [docs/architecture/DESIGN.md](docs/architecture/DESIGN.md) for the architecture and [docs/planning/ROADMAP.md](docs/planning/ROADMAP.md) for
planned work.

## Making a change

Keep changes small enough to review and avoid unrelated formatting or
refactoring. Preserve public API compatibility unless an approved proposal
explicitly calls for a breaking change.

When implementing PDF behavior:

- cite the relevant ISO 32000 section or other authoritative source in code or
  the pull request when the rule is subtle;
- handle malformed and adversarial input without panics or unbounded resource
  use;
- add a focused regression test close to the affected crate;
- test both success and failure paths, especially around lengths, offsets,
  recursion, allocation, decompression, encryption, and file writes;
- keep fixtures minimal, explain their origin, and avoid copyrighted or
  confidential documents.

If the change affects parser surfaces, consult [fuzz/README.md](fuzz/README.md)
and add or update a fuzz target or seed when appropriate.

## Quality gates

Run the same core checks as CI before requesting review:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Also run the narrowest relevant checks while iterating, for example:

```bash
cargo test -p zpdf-parser
cargo test -p zpdf-writer
cargo build -p zpdf --features gpu-render
cargo test -p zpdf --features gpu-render
```

If a platform-specific or expensive check cannot be run locally, say so in the
pull request and rely on the corresponding CI job. Do not claim a check passed
unless you ran it or a linked CI run did.

## Tests, documentation, and changelog

- Bug fixes should include a regression test that fails without the fix.
- New behavior should cover normal, boundary, and malformed inputs.
- Performance changes should include a repeatable measurement and describe the
  data set, build profile, and hardware.
- Public API or CLI changes require examples or documentation updates.
- User-visible changes belong in the `Unreleased` section of
  [docs/CHANGELOG.md](docs/CHANGELOG.md).

## Commits and pull requests

Use concise, imperative commit messages and pull request titles. The project
convention is:

```text
type(scope): summary
```

Common types are `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `chore`, and
`ci`. Use a scope such as `parser`, `writer`, `render`, `cli`, or `wasm` when it
helps reviewers route the change.

A good pull request:

- explains the user or technical problem and why the chosen design fits;
- links the issue with `Fixes #123` or explains why no issue is needed;
- records exact validation commands and useful evidence;
- calls out public API, compatibility, performance, memory, and security impact;
- keeps generated files and dependency changes intentional;
- responds to review with follow-up commits rather than rewriting unrelated
  history during active review.

Draft pull requests are welcome for early design or CI feedback. Mark the pull
request ready only when its description is complete and the relevant local
checks pass.

## Review and merge

Maintainers may ask for a smaller scope, additional tests, specification
references, or changes to public API design. CI success is necessary but does
not replace review. Pull requests may be squash-merged so the final title should
be suitable for release notes.

By contributing, you agree that your contribution is licensed under the
project's [MIT License](LICENSE) and that you have the right to submit it.
