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
its labels to Forgejo. On Forgejo 17.0.1, the disabled-workflow filter is not
applied to target-branch `pull_request_target` workflows. Disabling `triage.yml`
in the Actions page alone therefore does not stop these runs.

To validate the migration PR while it remains open, first land the GitHub-only
job condition from `.github/workflows/triage.yml` on the target branch, then
sync that branch to Forgejo before updating the PR. Keep the native `ci.yml`
enabled and verify checks against the new PR revision. Once the native
workflow directory reaches `main`, Forgejo no longer falls back to the GitHub
workflows. Historical failures remain recorded on their original revisions.
