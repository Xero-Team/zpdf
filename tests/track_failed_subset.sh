#!/usr/bin/env bash
# Track the in-repo slice of the tests/failed corpus.
#
# Why a script: the root .gitignore excludes /tests wholesale, and one fixture
# in tests/failed is 269 MB — over GitHub's 100 MB per-file push limit — so the
# whole directory can never be tracked. The rule we settled on is by SIZE:
#
#   * < 1 MiB  -> tracked in git (497 of 618 files, ~30 MB)
#   * >= 1 MiB -> local-only, identity pinned in tests/failed-manifest.tsv
#
# `git add -f` is required because /tests is ignored; already-tracked files are
# unaffected by ignore rules, so they behave normally once added. Nothing is
# committed here — this only stages. Review, then commit with your own
# `## Human note` (see AI_POLICY.md).
#
# Usage:
#   bash tests/track_failed_subset.sh              # stage the slice
#   bash tests/track_failed_subset.sh --check      # report only, stage nothing
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS="$ROOT/tests/failed"
MANIFEST="$ROOT/tests/failed-manifest.tsv"
LIMIT=1048576 # 1 MiB, matching FAILED_IN_REPO_MAX_BYTES in the manifest tool

CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1

if [ ! -d "$CORPUS" ]; then
    echo "track_failed_subset: $CORPUS is missing" >&2
    exit 1
fi

# Sizes come from `stat` per file rather than `find -printf`, which is not
# portable to the BSD find on macOS.
count=0
bytes=0
TMPFILE="$(mktemp)"
trap 'rm -f "$TMPFILE"' EXIT
: >"$TMPFILE"

while IFS= read -r -d '' pdf; do
    size=$(stat -c %s "$pdf" 2>/dev/null || stat -f %z "$pdf")
    if [ "$size" -lt "$LIMIT" ]; then
        printf '%s\0' "$pdf" >>"$TMPFILE"
        count=$((count + 1))
        bytes=$((bytes + size))
    fi
done < <(find "$CORPUS" -type f -name '*.pdf' -print0)

printf 'in-repo slice: %d files, %d bytes (%.1f MiB)\n' \
    "$count" "$bytes" "$(echo "$bytes" | awk '{print $1/1048576}')"

if [ "$CHECK" -eq 1 ]; then
    echo "check only — nothing staged"
    exit 0
fi

xargs -0 git -C "$ROOT" add -f <"$TMPFILE"
echo "staged $count files under tests/failed/"

# The manifest is the source of truth for what is local-only. If it is missing
# or stale, the robustness harness cannot tell a genuinely absent fixture from
# one that was never there.
if [ -f "$MANIFEST" ]; then
    echo "manifest present: tests/failed-manifest.tsv ($(grep -c -v '^#' "$MANIFEST") entries)"
else
    echo "WARNING: $MANIFEST missing — regenerate with:" >&2
    echo "  cargo run -p zpdf-benches --bin corpus-manifest -- --update" >&2
fi
