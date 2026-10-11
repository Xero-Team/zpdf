#!/usr/bin/env python3
"""Acquire pinned public Adobe mapping sources for offline reproducible generation.

This maintenance tool uses the GitHub CLI; rendering never invokes it. Sources
are stored byte-for-byte in a deterministic ZIP with per-file identities.
"""

import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import re
import subprocess
import zipfile

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "crates/zpdf-font/data/adobe-cmaps"
SOURCES = {
    "cmap-resources": "f5cf3bca7fdfeaceb77aa82847e974f2306c20b4",
    "mapping-resources-pdf": "2dd5e53fb74a01718b9dfd448a0d1cce6fff2aa5",
}
COLLECTIONS = {"Adobe-CNS1-7", "Adobe-GB1-6", "Adobe-Japan1-7", "Adobe-Korea1-2", "Adobe-KR-9"}
UCS_MAPS = {"Adobe-CNS1-UCS2", "Adobe-GB1-UCS2", "Adobe-Japan1-UCS2", "Adobe-Korea1-UCS2", "Adobe-KR-UCS2"}


def api(path):
    return json.loads(subprocess.check_output(["gh", "api", path], timeout=90))


def supported(name):
    if not name.endswith(("-H", "-V")):
        return False
    if name.startswith(("UniGB", "UniCNS", "UniJIS", "UniKS", "UniAKR")):
        return "UCS2" in name or "UTF16" in name
    return ("RKSJ" in name or name.startswith(("GBK", "GBpc", "GB-EUC", "B5pc"))
            or "-B5" in name or name in ("B5-H", "B5-V", "EUC-H", "EUC-V")
            or (name.startswith("KSC") and "Johab" not in name))


def fetch_entry(entry):
    repo, path, size, blob = entry
    response = api(f"repos/adobe-type-tools/{repo}/contents/{path}?ref={SOURCES[repo]}")
    if response.get("encoding") != "base64" or size > 1024 * 1024:
        raise ValueError(f"Unsupported resource representation: {path}")
    raw = base64.b64decode(response["content"], validate=False)
    actual = hashlib.sha1(f"blob {len(raw)}\0".encode() + raw).hexdigest()
    if len(raw) != size or actual != blob:
        raise ValueError(f"Source identity mismatch: {path}")
    return f"{repo}/{path}", raw, {
        "repository": f"adobe-type-tools/{repo}", "revision": SOURCES[repo],
        "path": path, "git_blob": blob, "size": size,
        "sha256": hashlib.sha256(raw).hexdigest(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, default=OUTPUT)
    args = parser.parse_args()
    all_entries = {}
    pending = []
    for repo, revision in SOURCES.items():
        tree = api(f"repos/adobe-type-tools/{repo}/git/trees/{revision}?recursive=1")
        if tree.get("truncated"):
            raise ValueError("Source inventory is truncated")
        for entry in tree["tree"]:
            if entry["type"] != "blob":
                continue
            path = entry["path"]
            item = (repo, path, entry["size"], entry["sha"])
            all_entries[(repo, Path(path).name)] = item
            parts = Path(path).parts
            if (repo == "cmap-resources" and len(parts) == 3 and parts[0] in COLLECTIONS
                    and parts[1] == "CMap" and supported(parts[2])):
                pending.append(item)
            elif repo == "mapping-resources-pdf" and path.startswith("pdf2unicode/") and parts[-1] in UCS_MAPS:
                pending.append(item)
            elif path in ("LICENSE.md", "LICENSE.txt"):
                pending.append(item)
    resources = {}
    metadata = {}
    while pending:
        with ThreadPoolExecutor(max_workers=4) as pool:
            for name, raw, info in pool.map(fetch_entry, sorted(set(pending))):
                resources[name] = raw
                metadata[name] = info
        pending = []
        for name, raw in list(resources.items()):
            repo = name.split("/", 1)[0]
            for base in re.findall(rb"/([^\s/]+)\s+usecmap", raw):
                item = all_entries.get((repo, base.decode("ascii")))
                if item is None:
                    raise ValueError(f"Missing inherited mapping: {name} -> {base!r}")
                key = f"{repo}/{item[1]}"
                if key not in resources:
                    pending.append(item)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    destination = args.output_dir / "sources.zip"
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name in sorted(resources):
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            archive.writestr(info, resources[name], compresslevel=9)
    manifest = {"schema": 1, "upstream_revisions": SOURCES, "files": metadata}
    (args.output_dir / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    (args.output_dir / "LICENSE.txt").write_bytes(resources["mapping-resources-pdf/LICENSE.txt"])
    print(f"Saved {len(resources)} verified source files; {destination.stat().st_size} archive bytes")


if __name__ == "__main__":
    main()
