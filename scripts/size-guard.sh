#!/usr/bin/env bash
# size-guard.sh — fail if any tracked Rust/Swift/Kotlin source file exceeds the
# 800-line ceiling (LOOP.md: "Keep every file < 800 lines"). Keeps the core auditable
# and forces decomposition before files become unreviewable.
#
# Counts only git-tracked *.rs / *.swift / *.kt files, so generated bindings and build
# artifacts (which are git-ignored) are never flagged. Runs in CI and locally.
set -euo pipefail

MAX=800
fail=0

# List tracked source files; -z + read -d handles paths with spaces safely.
while IFS= read -r -d '' file; do
    [ -f "$file" ] || continue
    lines=$(wc -l < "$file" | tr -d '[:space:]')
    if [ "$lines" -gt "$MAX" ]; then
        echo "FAIL: $file has $lines lines (> $MAX)"
        fail=1
    fi
done < <(git ls-files -z '*.rs' '*.swift' '*.kt')

if [ "$fail" -ne 0 ]; then
    echo "size-guard: at least one source file exceeds $MAX lines."
    exit 1
fi

echo "size-guard: OK — all tracked .rs/.swift/.kt files are <= $MAX lines."
