# Forgejo CI

Forgejo reads `.forgejo/workflows` instead of falling back to `.github/workflows`.
The Linux workflow runs the checks from GitHub's `ci.yml` and `bench.yml` in one
job so the shared runner can reuse compiled dependencies. Keep their checks in
sync when changing CI. Windows coverage remains in GitHub's CI matrix; there is
no Windows runner on this Forgejo instance.

The `ubuntu-latest` label currently selects the instance's Debian-based
`xero-bot-ci:rust-1.99-node20` image. It must provide Rust, rustfmt, Clippy, Node,
Python, and an account that can install packages with `apt-get` (root or sudo).
The workflow installs the GUI libraries and Mesa's software Vulkan driver,
then rejects skipped GPU oracle tests. Cargo builds use two workers to limit
pressure on the shared host.

Use fully qualified URLs for external actions. Unqualified GitHub action names
such as `dtolnay/rust-toolchain` otherwise resolve against Forgejo's configured
default action host, where they may not exist. This workflow uses the runner's
installed Rust toolchain; update the runner image when updating Rust.

CI runs for pull requests, pushes to `main`, `staging`, or `automation/xero/**`,
and manual dispatches for a selected branch. GitHub publishing, release,
scheduled fuzzing, and issue automation remain in `.github/workflows`; this
directory does not enable a second publisher or duplicate scheduled jobs.

## Migrating the target branch

Forgejo resolves `pull_request_target` workflows from the PR's target branch.
Until `main` contains `.forgejo/workflows`, it still falls back to
`.github/workflows/triage.yml`, even when the PR already uses the native CI.
Changing that workflow only in the PR does not change the target-branch run.

Triage uses GitHub API actions and runs only on GitHub. The bridge synchronizes
its labels to Forgejo. During migration, disable only `triage.yml` in the
Forgejo repository's Actions page before pushing the next PR revision. Keep
the native `ci.yml` enabled and verify its run for the new commit. Disabling
Triage prevents new runs; it does not erase failures recorded on older commits.
The GitHub-only job condition also prevents accidental execution if a later
branch falls back to `.github/workflows`.
