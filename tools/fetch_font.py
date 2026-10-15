#!/usr/bin/env python3
"""Opt-in host acquisition of a pinned raw font into a persistent zlib cache.

Only caller-selected HTTPS URLs are fetched. The renderer never calls this
tool. Cache identity is the SHA-256 of the original, uncompressed font bytes.
"""

import argparse
import hashlib
import os
import re
import sys
import tempfile
import urllib.parse
import urllib.request
import zlib
from pathlib import Path

CHUNK = 64 * 1024


def require_https(url):
    parts = urllib.parse.urlsplit(url)
    if (parts.scheme != "https" or not parts.hostname or parts.username
            or parts.password or parts.fragment):
        raise ValueError("Font source and redirects must use HTTPS without credentials or fragments")
    return url


class HttpsRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        require_https(newurl)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def validate_cache(path, expected_sha256, max_bytes):
    """Verify cached bytes with bounded decompression, including offline reads."""
    decoder = zlib.decompressobj()
    digest = hashlib.sha256()
    decoded = 0
    # zlib overhead for incompressible data, with a conservative fixed margin.
    encoded_limit = max_bytes + max_bytes // 1000 + CHUNK
    encoded = 0
    try:
        with path.open("rb") as source:
            while data := source.read(CHUNK):
                encoded += len(data)
                if encoded > encoded_limit or decoder.eof:
                    raise ValueError("Oversized cache or trailing font cache data")
                while data:
                    output = decoder.decompress(data, min(CHUNK, max_bytes - decoded + 1))
                    decoded += len(output)
                    if decoded > max_bytes:
                        raise ValueError("Cached font exceeds the decoded byte limit")
                    digest.update(output)
                    if decoder.unused_data:
                        raise ValueError("Trailing font cache data")
                    data = decoder.unconsumed_tail
        if not decoder.eof or decoded == 0 or digest.hexdigest() != expected_sha256:
            raise ValueError("Cached font is truncated or has the wrong SHA-256")
    except zlib.error as error:
        raise ValueError("Invalid zlib font cache") from error


def acquire(url, expected_sha256, cache_dir, max_bytes, offline=False, timeout=30):
    require_https(url)
    if not re.fullmatch(r"[0-9a-f]{64}", expected_sha256):
        raise ValueError("SHA-256 must contain exactly 64 lowercase hexadecimal characters")
    if max_bytes <= 0 or timeout <= 0:
        raise ValueError("Byte limit and timeout must be positive")
    cache_dir = Path(cache_dir)
    destination = cache_dir / (expected_sha256 + ".font.zlib")
    if destination.exists():
        try:
            validate_cache(destination, expected_sha256, max_bytes)
            return destination.resolve()
        except ValueError:
            if offline:
                raise
    if offline:
        raise ValueError("No verified cached font is available offline")
    cache_dir.mkdir(parents=True, exist_ok=True)
    request = urllib.request.Request(url, headers={"Accept-Encoding": "identity"})
    opener = urllib.request.build_opener(HttpsRedirect())
    temporary = None
    try:
        with opener.open(request, timeout=timeout) as response:
            require_https(response.geturl())
            if response.headers.get("Content-Encoding", "identity").lower() != "identity":
                raise ValueError("Font source must return uncompressed font bytes")
            length = response.headers.get("Content-Length")
            if length is not None and (int(length) < 0 or int(length) > max_bytes):
                raise ValueError("Font download exceeds the byte limit")
            digest = hashlib.sha256()
            compressor = zlib.compressobj()
            total = 0
            with tempfile.NamedTemporaryFile(dir=cache_dir, prefix="font-", delete=False) as output:
                temporary = Path(output.name)
                while data := response.read(min(CHUNK, max_bytes - total + 1)):
                    total += len(data)
                    if total > max_bytes:
                        raise ValueError("Font download exceeds the byte limit")
                    digest.update(data)
                    output.write(compressor.compress(data))
                output.write(compressor.flush())
            if total == 0 or digest.hexdigest() != expected_sha256:
                raise ValueError("Downloaded font has the wrong SHA-256")
        # No partial download is ever published as a usable cache entry.
        os.replace(temporary, destination)
        temporary = None
        return destination.resolve()
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--cache-dir", type=Path, required=True)
    parser.add_argument("--max-bytes", type=int, default=64 * 1024 * 1024)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    try:
        print(acquire(args.url, args.sha256, args.cache_dir, args.max_bytes,
                      offline=args.offline, timeout=args.timeout))
    except (OSError, ValueError) as error:
        print(f"fetch-font: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
