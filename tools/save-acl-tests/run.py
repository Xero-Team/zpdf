"""Run the production save module offline, bounded and without inherited env."""
import argparse
from enum import Enum
import json
import os
from pathlib import Path
import resource
import selectors
import signal
import subprocess
import sys
import time


class Task(Enum):
    RUNNER_TESTS = "runner regression tests"
    VERSION = "Rust version"
    PORTABLE = "portable and default native tests"
    NATIVE = "required FreeBSD fixture tests"


class ChildFailure(RuntimeError):
    def __init__(self, message, output):
        super().__init__(message)
        # Failure diagnostics have a separate, smaller bound.
        self.output = output[:64 * 1024]


def collect_child(process, *, output_limit=4 * 1024**2, wall_seconds=180, monitor=None):
    """Drain a new process group without unbounded communicate() buffering."""
    output = bytearray()
    deadline = time.monotonic() + wall_seconds
    next_monitor = 0.0
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while selector.get_map() or process.poll() is None:
                now = time.monotonic()
                if now >= deadline:
                    raise RuntimeError(f"native ACL child exceeded {wall_seconds} seconds")
                # Check even when a child continuously fills the output pipe.
                if monitor is not None and now >= next_monitor:
                    monitor(process.pid)
                    next_monitor = time.monotonic() + 0.5
                if not selector.get_map():
                    # EOF does not imply that the process has exited.
                    time.sleep(min(0.05, max(0, deadline - time.monotonic())))
                    continue
                for key, _ in selector.select(min(0.1, max(0, deadline - time.monotonic()))):
                    chunk = os.read(key.fileobj.fileno(), 64 * 1024)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    remaining = output_limit - len(output)
                    output.extend(chunk[:remaining])
                    if len(chunk) > remaining:
                        raise RuntimeError(f"native ACL child output exceeded {output_limit} bytes")
        if process.returncode:
            raise RuntimeError(f"native ACL child exited with status {process.returncode}")
        return output.decode("utf-8", errors="replace")
    except (RuntimeError, OSError) as error:
        raise ChildFailure(str(error), bytes(output[:64 * 1024])) from error
    finally:
        # Also remove descendants after normal completion or a closed pipe.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=5)
        process.stdout.close()


def monitor_darwin_group(pid):
    listing = subprocess.check_output(
        ["/bin/ps", "-axo", "pgid=,rss="], text=True,
        env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"}, timeout=5,
    )
    members = [int(rss) for group, rss in
               (line.split() for line in listing.splitlines()) if int(group) == pid]
    if len(members) > 128:
        raise RuntimeError("native ACL process group exceeded 128 processes")
    if sum(members) > 4 * 1024**2:
        raise RuntimeError("native ACL process group exceeded 4 GiB RSS")


def validate_runtime(args):
    if os.geteuid() == 0:
        raise ValueError("ACL behavior must be tested without root privileges")
    if sys.platform == "darwin":
        if args.sandbox is None or not args.sandbox.is_file():
            raise ValueError("macOS requires the provisioned Seatbelt profile")
        # Only the runner's installed rustup toolchain is an executable input.
        root = (Path.home() / ".rustup/toolchains").resolve(strict=True)
        cargo = args.cargo.resolve(strict=True)
        relative = cargo.relative_to(root)
        if len(relative.parts) != 3 or relative.parts[1:] != ("bin", "cargo"):
            raise ValueError("cargo must belong to an installed rustup toolchain")
        rustc = cargo.with_name("rustc").resolve(strict=True)
        if rustc.parent != cargo.parent or not all(
            tool.is_file() and os.access(tool, os.X_OK) for tool in (cargo, rustc)
        ):
            raise ValueError("cargo and rustc must be executable siblings in the toolchain")
        args.cargo = cargo
    elif sys.platform.startswith("freebsd"):
        if args.sandbox is not None or args.cargo != Path("/usr/local/bin/cargo"):
            raise ValueError("FreeBSD uses the fixed system toolchain inside the jail")
    else:
        raise ValueError("native ACL tests require macOS or FreeBSD")


def start_task(task, args, env, limits):
    """Launch only fixed CI tasks; no caller-supplied command or shell."""
    if task is Task.RUNNER_TESTS:
        arguments = ["-B", "-m", "unittest", "discover", "-s", "tools/save-acl-tests",
                     "-p", "test_run.py", "-v"]
    elif task is Task.VERSION:
        arguments = ["-vV"]
    elif task in (Task.PORTABLE, Task.NATIVE):
        arguments = ["test", "--frozen", "--manifest-path",
                     "tools/save-acl-tests/Cargo.toml", "--lib", "--"]
        if task is Task.NATIVE:
            arguments += ["native_acl_tests::", "--include-ignored"]
        arguments += ["--nocapture"]
    else:
        raise ValueError("unknown native ACL task")
    options = dict(cwd=args.source, env=env, preexec_fn=limits,
                   start_new_session=True, stdin=subprocess.DEVNULL,
                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, shell=False)
    if sys.platform == "darwin":
        if args.sandbox is None:
            raise ValueError("macOS requires the provisioned Seatbelt profile")
        executable = ("/usr/bin/python3" if task is Task.RUNNER_TESTS else
                      str(args.cargo.with_name("rustc" if task is Task.VERSION else "cargo")))
        return subprocess.Popen(
            ["/usr/bin/sandbox-exec", "-f", str(args.sandbox), executable, *arguments], **options,
        )
    if sys.platform.startswith("freebsd"):
        if task is Task.RUNNER_TESTS:
            return subprocess.Popen(["/usr/local/bin/python3.11", *arguments], **options)
        if task is Task.VERSION:
            return subprocess.Popen(["/usr/local/bin/rustc", *arguments], **options)
        return subprocess.Popen(["/usr/local/bin/cargo", *arguments], **options)
    raise ValueError("native ACL tests require macOS or FreeBSD")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--vendor", type=Path, required=True)
    parser.add_argument("--cargo", type=Path, required=True)
    parser.add_argument("--sandbox", type=Path)
    args = parser.parse_args()
    validate_runtime(args)
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
    if sys.platform == "darwin":
        # Apple's /usr/bin developer-tool shims otherwise write an xcrun cache
        # in the per-user system temp directory, outside the allowed scratch.
        env["xcrun_nocache"] = "1"
    if sys.platform.startswith("freebsd"):
        env["ZPDF_ACL_FIXTURES"] = str(args.scratch / "fixtures")
        env["TMPDIR"] = str(args.scratch / "fixtures/disabled/tmp")
        Path(env["TMPDIR"]).mkdir()
        env["LD_LIBRARY_PATH"] = "/usr/local/lib"

    # Darwin shares its uid with runner services; reserve slots above baseline.
    process_limit = 128
    if sys.platform == "darwin":
        users = subprocess.check_output(
            ["/bin/ps", "-axo", "uid="], text=True,
            env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"}, timeout=5,
        )
        process_limit += sum(int(uid) == os.geteuid() for uid in users.split())

    def limits():
        for kind, value in [
            (resource.RLIMIT_CPU, 120),
            (resource.RLIMIT_NPROC, process_limit),
            (resource.RLIMIT_NOFILE, 256),
            (resource.RLIMIT_FSIZE, 128 * 1024**2),
            (resource.RLIMIT_CORE, 0),
        ]:
            resource.setrlimit(kind, (value, value))
        # Darwin rejects AS/DATA/RSS setters; the parent monitors group RSS.
        if sys.platform.startswith("freebsd"):
            resource.setrlimit(resource.RLIMIT_AS, (4 * 1024**3, 4 * 1024**3))

    def run(task):
        print("Running:", task.value, flush=True)
        process = start_task(task, args, env, limits)
        output = collect_child(
            process, monitor=monitor_darwin_group if sys.platform == "darwin" else None,
        )
        print(output, flush=True)
        return output

    print("Native ACL runtime:", os.uname(), "uid=", os.geteuid(), flush=True)
    run(Task.RUNNER_TESTS)
    run(Task.VERSION)
    general = run(Task.PORTABLE)
    required = ["tests::commit_preserves_existing_permissions_after_private_writes",
                "tests::new_destination_remains_private_after_commit"]
    for test in required:
        if f"test {test} ... ok" not in general:
            raise RuntimeError(f"required portable test missing: {test}")
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
        native = run(Task.NATIVE)
    for test in expected:
        if f"test native_acl_tests::{test} ... ok" not in native:
            raise RuntimeError(f"required native test missing: {test}")
    print(f"Verified {len(expected)} mandatory native ACL cases; none skipped.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except ChildFailure as error:
        print(error.output.decode("utf-8", errors="replace"), file=sys.stderr, flush=True)
        sys.exit(str(error))
