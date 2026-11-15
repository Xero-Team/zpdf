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
limits, a 180-second deadline, and cleanup of its original process group. The parent drains
stdout/stderr incrementally, rejects output above 4 MiB per child and retains at
most 64 KiB of failure diagnostics. Deadlines and RSS checks apply even to a
child that continuously writes output or closes stdout before exiting.
FreeBSD uses a 4 GiB
address-space limit in a two-CPU, 4 GiB VM. Darwin rejects address-space limits:
the parent monitors aggregate process-group RSS with a 4 GiB kill threshold,
in addition to the hosted VM's memory bound. Darwin's per-user process limit
reserves 128 slots above the runner's existing service processes; the parent
also enforces a 128-process limit on the tested process group. Tests use dummy ACL principals and no
repository secrets; checkout does not persist credentials.

The launcher accepts only fixed version, runner-regression and Cargo test tasks.
macOS requires the provisioned Seatbelt profile and an executable Cargo/rustc
pair in the runner's installed rustup toolchain directory. FreeBSD uses fixed
system executable paths inside the jail. All invocations use argv lists with
the shell disabled. The runner's regression tests run inside the same sandbox
or jail before Rust tests; small finite fixtures cover output boundaries,
timeouts, process-group cleanup and rejection of unsupported launch inputs.

The workflow and provisioning/launcher scripts themselves are reviewed CI code.
The per-test sandbox constrains the child build/test tasks; it does not contain
an adversarial PR that changes those scripts or the workflow itself. The outer
isolation boundary for this PR workflow is the disposable GitHub-hosted VM.
This workflow uses read-only repository permissions, no repository secrets and
no persistent checkout credentials. It is not intended for a self-hosted runner
or a privileged workflow that executes unreviewed PR code.

Likewise, the 4 GiB Darwin RSS monitor and cleanup cover the original process
group, not descendants that deliberately create another session/group. They
are early failure controls, not an inescapable process-tree memory quota. The
hosted VM's memory allocation and the job timeout (15 minutes on macOS,
25 minutes for the FreeBSD VM job) provide the outer resource/lifetime bounds.
FreeBSD additionally bounds its VM to two CPUs/4 GiB and removes the entire jail
during cleanup. Stronger isolation against malicious changes to CI infrastructure
requires a separately trusted runner policy, outside this test harness.

These checks establish native save-policy behavior on the listed OS/filesystem
combinations. They do not validate GUI interaction, network filesystems, every
macOS/FreeBSD release, or support for preserving nontrivial FreeBSD ACLs (which
remains intentionally rejected).
