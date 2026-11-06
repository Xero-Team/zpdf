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
