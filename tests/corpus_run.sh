#!/usr/bin/env bash
# Run every veraPDF-corpus PDF through `zpdf render` and record the outcome.
# Output: TSV  status<TAB>relative-path<TAB>message
#   status: OK | FAIL | TIMEOUT | PANIC
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/zpdf"
CORPUS="$ROOT/tests/veraPDF-corpus"
OUT="${1:-$ROOT/tests/corpus_results.tsv}"
TIMEOUT_SECS=20

run_one() {
    local pdf="$1"
    local rel="${pdf#"$CORPUS/"}"
    local tmp_png err msg status rc
    tmp_png="$(mktemp /tmp/zpdf-corpus-XXXXXX.png)"
    err="$(timeout "$TIMEOUT_SECS" "$BIN" render "$pdf" -p 1 -o "$tmp_png" --dpi 72 2>&1 >/dev/null)"
    rc=$?
    rm -f "$tmp_png"
    if [ $rc -eq 0 ]; then
        status="OK"; msg=""
    elif [ $rc -eq 124 ]; then
        status="TIMEOUT"; msg="exceeded ${TIMEOUT_SECS}s"
    else
        # Distinguish rust panics from clean Error exits
        if printf '%s' "$err" | grep -q "panicked at"; then
            status="PANIC"
        else
            status="FAIL"
        fi
        msg="$(printf '%s' "$err" | grep -E "^(Error:|thread .*panicked)" | head -1)"
        [ -z "$msg" ] && msg="$(printf '%s' "$err" | tail -1)"
    fi
    printf '%s\t%s\t%s\n' "$status" "$rel" "$msg"
}

export BIN CORPUS TIMEOUT_SECS
export -f run_one

find "$CORPUS" -iname '*.pdf' -print0 | sort -z \
    | xargs -0 -P 32 -I{} bash -c 'run_one "$@"' _ {} > "$OUT"

total=$(wc -l < "$OUT")
ok=$(grep -c $'^OK\t' "$OUT" || true)
echo "done: $total files, $ok OK, $((total - ok)) not OK -> $OUT" >&2
