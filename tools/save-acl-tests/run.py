"""Run the production save module offline, bounded and without inherited env."""
import argparse
import json
import os
from pathlib import Path
import resource
import signal
import subprocess
import sys

parser = argparse.ArgumentParser()
parser.add_argument("--source", type=Path, required=True)
parser.add_argument("--scratch", type=Path, required=True)
parser.add_argument("--vendor", type=Path, required=True)
parser.add_argument("--cargo", type=Path, required=True)
parser.add_argument("--sandbox", type=Path)
args = parser.parse_args()
assert os.geteuid() != 0, "ACL behavior must be tested without root privileges"
assert sys.platform == "darwin" or sys.platform.startswith("freebsd")

for directory in ["home", "tmp", "cargo", "target"]:
    (args.scratch / directory).mkdir(exist_ok=True)
(args.scratch / "cargo/config.toml").write_text(
    '[source.crates-io]\nreplace-with = "acl-ci"\n'
    '[source.acl-ci]\ndirectory = ' + json.dumps(str(args.vendor)) + "\n"
)
env = {
    "PATH": f"{args.cargo.parent}:/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin",
    "HOME": str(args.scratch / "home"),
    "TMPDIR": str(args.scratch / "tmp"),
    "CARGO_HOME": str(args.scratch / "cargo"),
    "CARGO_TARGET_DIR": str(args.scratch / "target"),
    "CARGO_BUILD_JOBS": "2",
    "CARGO_TERM_COLOR": "never",
    "RUST_TEST_THREADS": "1",
    "LC_ALL": "C",
}
if sys.platform.startswith("freebsd"):
    env["ZPDF_ACL_FIXTURES"] = str(args.scratch / "fixtures")
    env["TMPDIR"] = str(args.scratch / "fixtures/disabled/tmp")
    Path(env["TMPDIR"]).mkdir()
    env["LD_LIBRARY_PATH"] = "/usr/local/lib"


def limits():
    for kind, value in [
        (resource.RLIMIT_CPU, 120),
        (resource.RLIMIT_AS, 4 * 1024**3),
        (resource.RLIMIT_NPROC, 128),
        (resource.RLIMIT_NOFILE, 256),
        (resource.RLIMIT_FSIZE, 128 * 1024**2),
        (resource.RLIMIT_CORE, 0),
    ]:
        resource.setrlimit(kind, (value, value))


def run(command):
    if args.sandbox:
        command = ["/usr/bin/sandbox-exec", "-f", str(args.sandbox), *command]
    print("Running:", " ".join(map(str, command)), flush=True)
    process = subprocess.Popen(
        command, cwd=args.source, env=env, preexec_fn=limits,
        start_new_session=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        output, _ = process.communicate(timeout=180)
    finally:
        # Also remove any descendants after normal completion.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    print(output, flush=True)
    if process.returncode:
        raise SystemExit(process.returncode)
    return output


print("Native ACL runtime:", os.uname(), "uid=", os.geteuid(), flush=True)
run([str(args.cargo.parent / "rustc"), "-vV"])
base = [str(args.cargo), "test", "--frozen", "--manifest-path",
        str(args.source / "tools/save-acl-tests/Cargo.toml"), "--lib"]
general = run([*base, "--", "--nocapture"])
assert "test tests::commit_preserves_existing_permissions_after_private_writes ... ok" in general
assert "test tests::new_destination_remains_private_after_commit ... ok" in general
if sys.platform == "darwin":
    expected = ["macos_preserves_ordered_allow_and_deny_acl",
                "macos_rejects_inherited_allow_before_writing",
                "macos_rejects_inherited_deny_before_writing"]
    native = general
else:
    expected = ["freebsd_accepts_trivial_posix_acl", "freebsd_accepts_trivial_nfs4_acl",
                "freebsd_accepts_confirmed_disabled_acls", "freebsd_rejects_nontrivial_posix_acl",
                "freebsd_rejects_nontrivial_nfs4_acl", "freebsd_rejects_inherited_posix_acl",
                "freebsd_rejects_inherited_nfs4_acl"]
    native = run([*base, "--", "native_acl_tests::", "--include-ignored", "--nocapture"])
for test in expected:
    assert f"test native_acl_tests::{test} ... ok" in native, f"required native test missing: {test}"
print(f"Verified {len(expected)} mandatory native ACL cases; none skipped.", flush=True)
