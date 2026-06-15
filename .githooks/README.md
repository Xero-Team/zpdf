# Git hooks

Version-controlled hooks for this repo. They mirror the CI gates so a push never
fails on something a local check would have caught.

## One-time setup (per clone)

```sh
git config core.hooksPath .githooks
```

That points git at this directory instead of `.git/hooks`. On Windows the hooks
run through Git's bundled `sh`, so no extra setup is needed.

## Hooks

- **`pre-commit`** — runs `cargo fmt --all --check` and
  `cargo clippy --workspace --all-targets -- -D warnings` (exactly what
  `.github/workflows/ci.yml` runs). It skips automatically when a commit touches
  no `.rs` / `.wgsl` / `Cargo.*` files.

Bypass a hook for a single commit with `git commit --no-verify`.
