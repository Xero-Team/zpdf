"""Small, finite child fixtures exercise the launcher's failure boundaries."""
import argparse
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import run


def child(script, *arguments):
    environment = {"PATH": "/usr/bin:/bin:/usr/local/bin", "LC_ALL": "C",
                   "HOME": os.environ["HOME"], "TMPDIR": tempfile.gettempdir()}
    if sys.platform.startswith("freebsd"):
        environment["LD_LIBRARY_PATH"] = "/usr/local/lib"
    options = dict(env=environment,
                   start_new_session=True, stdin=subprocess.DEVNULL,
                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, shell=False)
    if sys.platform.startswith("freebsd"):
        return subprocess.Popen(["/usr/local/bin/python3.11", "-I", "-c", script, *arguments],
                                **options)
    return subprocess.Popen(["python3", "-I", "-c", script, *arguments], **options)


class CaptureTests(unittest.TestCase):
    def test_exact_limit_accepts_both_output_streams(self):
        process = child('import os; os.write(1, b"a" * 512); os.write(2, b"b" * 512)')
        self.assertEqual(run.collect_child(process, output_limit=1024, wall_seconds=5),
                         "a" * 512 + "b" * 512)
        self.assertTrue(process.stdout.closed)

    def test_output_over_limit_fails_and_reaps(self):
        # Just 1025 bytes suffice to reproduce the formerly missing byte limit.
        process = child('import os; os.write(1, b"x" * 1025)')
        with self.assertRaisesRegex(run.ChildFailure, "output exceeded 1024 bytes") as caught:
            run.collect_child(process, output_limit=1024, wall_seconds=5)
        self.assertEqual(caught.exception.output, b"x" * 1024)
        self.assertIsNotNone(process.returncode)
        self.assertTrue(process.stdout.closed)

    def test_failure_diagnostics_have_a_smaller_bound(self):
        process = child('import sys; sys.stdout.write("x" * 131072); sys.exit(7)')
        with self.assertRaisesRegex(run.ChildFailure, "status 7") as caught:
            run.collect_child(process, wall_seconds=5)
        self.assertEqual(len(caught.exception.output), 64 * 1024)

    def test_invalid_utf8_is_bounded_and_decoded(self):
        process = child(r'import os; os.write(1, b"\xff\xc3\xa9")')
        self.assertEqual(run.collect_child(process, output_limit=3, wall_seconds=5), "\ufffdé")

    def test_deadline_applies_after_stdout_closes(self):
        process = child('import os, time; os.close(1); os.close(2); time.sleep(5)')
        with self.assertRaisesRegex(run.ChildFailure, "exceeded .* seconds"):
            run.collect_child(process, wall_seconds=0.2)
        self.assertEqual(process.returncode, -signal.SIGKILL)

    def test_deadline_applies_while_output_continues(self):
        process = child('import os, time\nfor _ in range(1000):\n os.write(1, b"x" * 64)\n time.sleep(0.005)')
        with self.assertRaisesRegex(run.ChildFailure, "exceeded .* seconds"):
            run.collect_child(process, wall_seconds=0.2)
        self.assertIsNotNone(process.returncode)

    def test_monitor_keeps_running_while_output_continues(self):
        calls = []

        def monitor(pid):
            calls.append(pid)
            if len(calls) == 2:
                raise RuntimeError("fixture RSS budget")

        process = child('import os, time\nfor _ in range(1000):\n os.write(1, b"x" * 64)\n time.sleep(0.005)')
        with self.assertRaisesRegex(run.ChildFailure, "fixture RSS budget"):
            run.collect_child(process, wall_seconds=5, monitor=monitor)
        self.assertEqual(calls, [process.pid, process.pid])
        self.assertIsNotNone(process.returncode)

    def test_descendants_are_killed_on_success_and_output_failure(self):
        script = '''import os, pathlib, sys, time
if os.fork() == 0:
    os.close(1)
    os.close(2)
    time.sleep(0.5)
    pathlib.Path(sys.argv[1]).write_text("descendant survived")
    os._exit(0)
os.write(1, b"x" * int(sys.argv[2]))
'''
        with tempfile.TemporaryDirectory() as directory:
            for size in (1, 1025):
                with self.subTest(size=size):
                    marker = Path(directory) / f"survived-{size}"
                    process = child(script, str(marker), str(size))
                    if size == 1:
                        self.assertEqual(run.collect_child(process, output_limit=1024,
                                                           wall_seconds=5), "x")
                    else:
                        with self.assertRaises(run.ChildFailure):
                            run.collect_child(process, output_limit=1024, wall_seconds=5)
                    time.sleep(0.7)
                    self.assertFalse(marker.exists())


class LaunchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        # Shell metacharacters remain literal path components.
        binary = self.home / ".rustup/toolchains/stable ; literal/bin"
        binary.mkdir(parents=True)
        for name in ("cargo", "rustc"):
            (binary / name).touch()
            (binary / name).chmod(0o700)
        profile = self.home / "profile ; literal.sb"
        profile.touch()
        self.args = argparse.Namespace(source=self.home, cargo=binary / "cargo", sandbox=profile)
        self.uid = mock.patch.object(run.os, "geteuid", return_value=65534)
        self.uid.start()
        self.addCleanup(self.uid.stop)

    def validate_macos(self):
        with mock.patch.object(run.sys, "platform", "darwin"), \
                mock.patch.object(run.Path, "home", return_value=self.home):
            run.validate_runtime(self.args)

    def test_macos_accepts_installed_toolchain_and_literal_paths(self):
        self.validate_macos()
        with mock.patch.object(run.sys, "platform", "darwin"), \
                mock.patch.object(run.subprocess, "Popen") as launch:
            run.start_task(run.Task.PORTABLE, self.args, {}, None)
        argv = launch.call_args.args[0]
        self.assertEqual(argv[:4], ["/usr/bin/sandbox-exec", "-f", str(self.args.sandbox),
                                   str(self.args.cargo)])
        self.assertIn("--frozen", argv)
        self.assertIs(launch.call_args.kwargs["shell"], False)

    def test_macos_requires_sandbox(self):
        self.args.sandbox = None
        with self.assertRaisesRegex(ValueError, "Seatbelt"):
            self.validate_macos()
        with mock.patch.object(run.sys, "platform", "darwin"), \
                mock.patch.object(run.subprocess, "Popen") as launch:
            with self.assertRaisesRegex(ValueError, "Seatbelt"):
                run.start_task(run.Task.PORTABLE, self.args, {}, None)
            launch.assert_not_called()

    def test_macos_rejects_external_and_symlinked_executables(self):
        outside = self.home / "cargo"
        outside.touch()
        outside.chmod(0o700)
        installed = self.args.cargo
        for path in (outside, installed):
            with self.subTest(path=path):
                if path == installed:
                    installed.unlink()
                    installed.symlink_to(outside)
                self.args.cargo = path
                with self.assertRaises(ValueError):
                    self.validate_macos()

    def test_macos_rejects_rustc_symlink_escape(self):
        outside = self.home / "rustc"
        outside.touch()
        outside.chmod(0o700)
        rustc = self.args.cargo.with_name("rustc")
        rustc.unlink()
        rustc.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "siblings"):
            self.validate_macos()

    def test_freebsd_uses_only_fixed_system_toolchain(self):
        with mock.patch.object(run.sys, "platform", "freebsd14"):
            with self.assertRaisesRegex(ValueError, "fixed system toolchain"):
                run.validate_runtime(self.args)
            self.args.cargo = Path("/usr/local/bin/cargo")
            self.args.sandbox = None
            run.validate_runtime(self.args)
            with mock.patch.object(run.subprocess, "Popen") as launch:
                run.start_task(run.Task.NATIVE, self.args, {}, None)
            argv = launch.call_args.args[0]
            self.assertEqual(argv[0], "/usr/local/bin/cargo")
            self.assertIn("--include-ignored", argv)

    def test_unknown_task_never_starts_a_process(self):
        with mock.patch.object(run.subprocess, "Popen") as launch:
            with self.assertRaisesRegex(ValueError, "unknown"):
                run.start_task(["sh", "-c", "exit 0"], self.args, {}, None)
            launch.assert_not_called()

    def test_root_and_unsupported_platform_are_rejected(self):
        with mock.patch.object(run.os, "geteuid", return_value=0):
            with self.assertRaisesRegex(ValueError, "root"):
                run.validate_runtime(self.args)
        with mock.patch.object(run.sys, "platform", "linux"):
            with self.assertRaisesRegex(ValueError, "macOS or FreeBSD"):
                run.validate_runtime(self.args)


if __name__ == "__main__":
    unittest.main()
