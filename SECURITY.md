# Security policy

zpdf parses and transforms untrusted, structurally complex documents. Please
report potential vulnerabilities privately so they can be investigated and
fixed before public disclosure.

## Supported versions

Security fixes are made for the latest released version and the current `main`
branch. Older releases may receive a fix only when maintainers determine that a
backport is practical.

| Version | Supported |
| --- | --- |
| Latest release | Yes |
| `main` | Yes |
| Older releases | Best effort |

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/Xero-Team/zpdf/security/advisories/new).
Do not open a public issue, pull request, or discussion containing vulnerability
details.

Include as much of the following as possible:

- the affected zpdf version or commit and enabled features;
- affected crate, API, CLI command, or export path;
- impact and a realistic attack scenario;
- minimal reproduction steps or a reduced proof-of-concept PDF;
- operating system, architecture, Rust version, and relevant limits;
- whether the issue is a regression and any known-good version;
- suggested mitigations or fixes, if available.

Remove credentials and personal information. If the reproducer originates from
a confidential document, create a synthetic or minimized equivalent before
uploading it.

Maintainers will acknowledge the report on a best-effort basis, assess impact
and affected versions, and coordinate remediation and disclosure with the
reporter. Please allow a reasonable period for investigation before publishing
details.

## Security-relevant areas

Reports are especially useful when they demonstrate a concrete impact involving:

- memory, CPU, recursion, decompression, or allocation exhaustion from an
  untrusted PDF;
- panics, hangs, out-of-bounds behavior in dependencies, or unsafe boundary
  violations reachable through zpdf;
- path traversal, unintended file writes, or malicious output during export or
  document rewriting;
- encryption, password handling, digital signatures, redaction, or permission
  enforcement failures;
- parser differentials that bypass a documented security decision;
- WebAssembly or GPU behavior that crosses an expected isolation boundary.

Unsupported PDF features, rendering differences without security impact, and
dependency advisories without a demonstrated reachable impact in zpdf should be
reported as normal bugs or dependency updates rather than vulnerabilities.

## Coordinated disclosure

When a report is accepted, maintainers may use a private security advisory to
develop the fix, request a CVE when appropriate, and prepare release notes. Give
credit preferences in the report; anonymous disclosure is also respected.
