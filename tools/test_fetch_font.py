"""Acquisition failures and offline reuse, with no live network dependency."""

import hashlib
import io
import tempfile
import unittest
import zlib
from pathlib import Path
from unittest.mock import Mock, patch

from fetch_font import HttpsRedirect, acquire, require_https, validate_cache

SOURCE = "https://example.invalid/pinned/font.ttf"
FONT = (Path(__file__).resolve().parents[1]
        / "crates/zpdf-font/tests/fixtures/coverage-primary.ttf").read_bytes()
SHA = hashlib.sha256(FONT).hexdigest()


class Response(io.BytesIO):
    def __init__(self, data, headers=None):
        super().__init__(data)
        self.headers = headers or {}

    def geturl(self):
        return SOURCE


class AcquisitionTests(unittest.TestCase):
    def test_verified_download_is_reused_offline(self):
        opener = Mock()
        opener.open.return_value = Response(FONT)
        with tempfile.TemporaryDirectory() as directory, patch(
                "urllib.request.build_opener", return_value=opener):
            path = acquire(SOURCE, SHA, directory, len(FONT))
            self.assertEqual(zlib.decompress(path.read_bytes()), FONT)
            self.assertEqual(acquire(SOURCE, SHA, directory, len(FONT), offline=True), path)
            opener.open.assert_called_once()

    def test_partial_wrong_and_oversized_downloads_never_publish(self):
        for data, headers in [(FONT[:-1], {}), (FONT + b"x", {}),
                              (FONT, {"Content-Length": str(len(FONT) + 1)}),
                              (FONT, {"Content-Encoding": "gzip"})]:
            with self.subTest(headers=headers, size=len(data)), tempfile.TemporaryDirectory() as directory:
                opener = Mock()
                opener.open.return_value = Response(data, headers)
                with patch("urllib.request.build_opener", return_value=opener):
                    with self.assertRaises(ValueError):
                        acquire(SOURCE, SHA, directory, len(FONT))
                self.assertEqual(list(Path(directory).iterdir()), [])

    def test_corrupt_offline_cache_fails_without_network(self):
        with tempfile.TemporaryDirectory() as directory, patch("urllib.request.build_opener") as network:
            path = Path(directory) / (SHA + ".font.zlib")
            for data in [b"broken", zlib.compress(FONT)[:-1], zlib.compress(FONT) + b"x",
                         zlib.compress(b"x" * 1_000_000)]:
                path.write_bytes(data)
                with self.assertRaises(ValueError):
                    acquire(SOURCE, SHA, directory, len(FONT), offline=True)
            network.assert_not_called()

    def test_missing_offline_cache_does_not_create_directory(self):
        with tempfile.TemporaryDirectory() as directory, patch("urllib.request.build_opener") as network:
            missing = Path(directory) / "missing"
            with self.assertRaises(ValueError):
                acquire(SOURCE, SHA, missing, len(FONT), offline=True)
            self.assertFalse(missing.exists())
            network.assert_not_called()

    def test_cached_hash_and_smaller_budget_are_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cache"
            path.write_bytes(zlib.compress(FONT))
            validate_cache(path, SHA, len(FONT))
            for digest, limit in [("0" * 64, len(FONT)), (SHA, len(FONT) - 1)]:
                with self.assertRaises(ValueError):
                    validate_cache(path, digest, limit)

    def test_https_is_required_for_source_and_redirect(self):
        for url in ["http://example.com/font", "file:///font", "https://u:p@example.com/font",
                    "https://example.com/font#fragment", "https:///font"]:
            with self.assertRaises(ValueError):
                require_https(url)
            with self.assertRaises(ValueError):
                HttpsRedirect().redirect_request(None, None, 302, "", {}, url)


if __name__ == "__main__":
    unittest.main()
