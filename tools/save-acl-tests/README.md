# Native save ACL checks

[Native save ACL](../../.github/workflows/save-acl.yml) compiles the production
`save_temp.rs` module directly with only its locked `libc` dependency. It runs on
every PR and main/staging push, and can also be dispatched manually. No desktop
GUI or cross-compilation is involved.

| Runner | Required kernel cases |
| --- | --- |
| macOS 15, Apple Silicon and Intel | Ordered allow/deny ACLs survive replacement; inherited allow and deny ACEs reject the empty temporary file before document writes. |
| FreeBSD 14.4 and 15.1, amd64 | Separate UFS mounts exercise POSIX ACLs, NFSv4 ACLs and explicitly disabled ACLs. Trivial ACLs and disabled ACLs allow saves; nontrivial destination and inherited ACLs fail without changing the destination or leaving a temporary file. |

The same jobs also run the portable save tests for private creation, atomic
replacement, mode preservation, symlinks, abandoned saves and error cleanup.
Native `chmod`/`ls` or `setfacl`/`getfacl` utilities create and inspect the fixtures
independently of the production ACL reader. FreeBSD tests verify both kernel
`fpathconf` capability flags before proceeding. Missing tools or ACL support fail
the job. The runner checks every expected native test name for an actual pass.
FreeBSD fixture tests are ignored in ordinary desktop test runs and explicitly
enabled by this workflow; the pre-existing supplementary-group test is outside
this job's scope.

Dependencies are provisioned before execution. Builds and tests then run offline
with a fresh environment and an unprivileged identity: a macOS Seatbelt sandbox
or a FreeBSD jail with IPv4/IPv6 disabled. Source and toolchains are read-only to
the tested process. Writable space is bounded to a 512 MiB APFS image or tmpfs
plus three 64 MiB UFS volumes. Each child has CPU, process, descriptor and file-size
limits, a 180-second deadline, and process-group cleanup. FreeBSD uses a 4 GiB
address-space limit in a two-CPU, 4 GiB VM. Darwin rejects address-space limits:
the parent monitors aggregate process-group RSS with a 4 GiB kill threshold,
in addition to the hosted VM's memory bound. Darwin's per-user process limit
reserves 128 slots above the runner's existing service processes; the parent
also enforces a 128-process limit on the tested process group. Tests use dummy ACL principals and no
repository secrets; checkout does not persist credentials.

These checks establish native save-policy behavior on the listed OS/filesystem
combinations. They do not validate GUI interaction, network filesystems, every
macOS/FreeBSD release, or support for preserving nontrivial FreeBSD ACLs (which
remains intentionally rejected).
