<!--
Thank you for contributing to zpdf.

Use a focused title in the form `type(scope): summary`, for example:
  fix(parser): reject truncated xref streams without panicking

Common types: feat, fix, perf, refactor, docs, test, chore, ci.
Keep the scope when it makes the affected crate or subsystem clearer.
-->

## Summary

<!-- What problem does this solve, and what changed? Focus on behavior and design rather than listing files. -->

## Related issue

<!-- Use `Fixes #123` to close an issue when this PR merges, or `Related: #123` when it should remain open. -->

Related: #

## Implementation notes

<!-- Explain important design decisions, PDF specification references, compatibility constraints, and deliberate non-goals. -->

## Validation

<!-- List the exact commands run and their results. Add before/after output, screenshots, benchmarks, or a minimal test PDF when relevant. -->

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Compatibility and risk

<!-- Note public API changes, changed rendering/writer output, performance or memory impact, platform differences, and migration needs. Write "None" if not applicable. -->

## Checklist

- [ ] The change is focused and does not include unrelated refactoring.
- [ ] I added or updated tests, or explained above why tests are not practical.
- [ ] I ran the relevant formatting, lint, build, and test commands.
- [ ] I updated public documentation and `docs/CHANGELOG.md` for user-visible changes.
- [ ] New or changed fixtures are minimal, redistributable, and contain no confidential data.
- [ ] I called out breaking API or behavior changes explicitly.
- [ ] If AI tools were used, I followed [AI_POLICY.md](../../AI_POLICY.md) and added a short human-written note in my own words (my mother tongue) at the bottom of this PR explaining what I intended to do.
