#!/bin/sh
# External conformance suite validation, regression corpus checks,
# and suite pass thresholds.
#
# Fetches the pinned revisions of the external test suites into a cache
# directory, runs every category ROM headless through the interpreter,
# and prints a per-category pass-rate report. The regression corpus
# (corpus.json) and the suite thresholds (tests/suite-thresholds.txt) gate
# the exit code: a determinism failure, a pinned state-hash mismatch, a
# frame-time budget regression, or a suite category below its minimum
# passed count fails the script and names the entry.
#
# Fetch policy: revisions are pinned below; suites are cloned over the
# network once into the cache and reused offline afterwards. When a
# suite cannot be fetched the script skips it loudly with instructions
# instead of failing.
#
# License record (the suites are never redistributed; only fetched):
#   jsmolka/gba-tests  MIT  Copyright (c) 2019-2020 JSMolka
#   mgba-emu/suite     MIT  Copyright (c) 2013-2016 Jeffrey Pfau
#
# The cache directory is .suite-cache/ (gitignored) or
# $OXYBOY_SUITE_CACHE; no suite artifact is ever committed.

set -eu

CACHE="${OXYBOY_SUITE_CACHE:-.suite-cache}"
GBA_TESTS_URL="https://github.com/jsmolka/gba-tests.git"
GBA_TESTS_REV="a7113b67e63f83a9b321696ddd7042ccfad6c881"
MGBA_SUITE_URL="https://github.com/mgba-emu/suite.git"
MGBA_SUITE_REV="e6942030d25ffe3ba76c72b73a86da073ec857cc"
FRAMES="${OXYBOY_SUITE_FRAMES:-600}"

# Scored categories: one ROM per category except save (one ROM per
# backup medium). ppu ships demo ROMs with no pass/fail protocol and is
# reported as unscored.
CATEGORIES="arm bios memory thumb unsafe"
SAVE_ROMS="none sram flash64 flash128"

fetch() {
    # fetch <dir> <url> <rev>: clone at the pinned revision, reusing an
    # up-to-date cache. Returns nonzero when the network is unusable.
    dir="$1"
    url="$2"
    rev="$3"
    if [ -d "$dir/.git" ]; then
        if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$rev" ]; then
            return 0
        fi
        rm -rf "$dir"
    fi
    mkdir -p "$(dirname "$dir")"
    git clone --quiet "$url" "$dir" || return 1
    git -C "$dir" checkout --quiet "$rev" || return 1
    actual="$(git -C "$dir" rev-parse HEAD)"
    if [ "$actual" != "$rev" ]; then
        echo "validate: pinned revision mismatch in $dir: $actual" >&2
        return 1
    fi
}

# The highest test number in a category directory is its test total
# (each suite test ends in `m_exit <n>`; tests are numbered
# consecutively).
total_tests() {
    grep -h "m_exit" "$1"/*.asm |
        sed -E 's/.*m_exit[[:space:]]+([0-9]+).*/\1/' |
        sort -n |
        tail -1
}

suite_test() {
    # suite_test <rom> <glyphs>: prints the verdict token of one ROM.
    if [ -n "${OXYBOY_BIN:-}" ]; then
        "$OXYBOY_BIN" suite-test --rom "$1" --glyphs "$2" --frames "$FRAMES"
    else
        cargo run --release --quiet -p oxyboy -- \
            suite-test --rom "$1" --glyphs "$2" --frames "$FRAMES"
    fi
}

corpus_bin() {
    if [ -n "${OXYBOY_BIN:-}" ]; then
        "$OXYBOY_BIN" "$@"
    else
        cargo run --release --quiet -p oxyboy -- "$@"
    fi
}

mkdir -p "$CACHE"
echo "== external suite validation =="
echo "cache: $CACHE"

mark_result() {
    # mark_result <var-prefix> <passed>: record a scored outcome for
    # the threshold gate at the end.
    eval "${1}_ran=1"
    eval "${1}_passed=$2"
}

GBA_DIR="$CACHE/gba-tests"
SUITE_RAN=0
if fetch "$GBA_DIR" "$GBA_TESTS_URL" "$GBA_TESTS_REV"; then
    SUITE_RAN=1
    echo "source: jsmolka/gba-tests @ $GBA_TESTS_REV (MIT)"
    pass_count=0
    scored=0
    for cat in $CATEGORIES; do
        rom="$GBA_DIR/$cat/$cat.gba"
        total="$(total_tests "$GBA_DIR/$cat")"
        if [ ! -f "$rom" ]; then
            echo "$cat: MISSING ($rom)"
            continue
        fi
        if verdict="$(suite_test "$rom" "$GBA_DIR/lib/glyphs.asm" 2>/dev/null)"; then
            case "$verdict" in
                pass)
                    echo "$cat: PASS $total/$total (100%)"
                    pass_count=$((pass_count + 1))
                    scored=$((scored + 1))
                    mark_result "$cat" "$total"
                    ;;
                fail\ *)
                    n="${verdict#fail }"
                    if [ -n "$total" ] && [ "$total" -gt 0 ]; then
                        pct=$(awk "BEGIN {printf \"%.1f\", ($n - 1) * 100 / $total}")
                        echo "$cat: FAIL at test $n ($((n - 1))/$total = $pct%)"
                    else
                        echo "$cat: FAIL at test $n"
                    fi
                    scored=$((scored + 1))
                    mark_result "$cat" "$((n - 1))"
                    ;;
                timeout)
                    echo "$cat: TIMEOUT (no verdict after $FRAMES frames)"
                    scored=$((scored + 1))
                    mark_result "$cat" 0
                    ;;
                *)
                    echo "$cat: UNEXPECTED verdict: $verdict"
                    mark_result "$cat" 0
                    ;;
            esac
        else
            echo "$cat: RUNNER FAILED (suite-test exited nonzero)"
            mark_result "$cat" 0
        fi
    done
    for rom in $SAVE_ROMS; do
        path="$GBA_DIR/save/$rom.gba"
        if [ ! -f "$path" ]; then
            echo "save/$rom: MISSING ($path)"
            continue
        fi
        total="$(total_tests "$GBA_DIR/save")"
        if verdict="$(suite_test "$path" "$GBA_DIR/lib/glyphs.asm" 2>/dev/null)"; then
            case "$verdict" in
                pass)
                    echo "save/$rom: PASS"
                    pass_count=$((pass_count + 1))
                    scored=$((scored + 1))
                    mark_result "save_$rom" 1
                    ;;
                fail\ *)
                    n="${verdict#fail }"
                    echo "save/$rom: FAIL at test $n (category total $total)"
                    scored=$((scored + 1))
                    # Save ROMs gate as pass or fail: a partial run is
                    # a failure, whatever test it stopped at.
                    mark_result "save_$rom" 0
                    ;;
                timeout)
                    echo "save/$rom: TIMEOUT (no verdict after $FRAMES frames)"
                    scored=$((scored + 1))
                    mark_result "save_$rom" 0
                    ;;
                *)
                    echo "save/$rom: UNEXPECTED verdict: $verdict"
                    mark_result "save_$rom" 0
                    ;;
            esac
        else
            echo "save/$rom: RUNNER FAILED (suite-test exited nonzero)"
            mark_result "save_$rom" 0
        fi
    done
    echo "ppu: NOT SCORED (demo ROMs; no pass/fail protocol)"
    echo "jsmolka summary: $pass_count/$scored scored units fully passing"
else
    echo "SKIP: jsmolka/gba-tests is unavailable (offline or GitHub"
    echo "  unreachable). Reconnect and rerun, or point"
    echo "  OXYBOY_SUITE_CACHE at a populated cache directory."
fi

echo

MGBA_DIR="$CACHE/mgba-suite"
if fetch "$MGBA_DIR" "$MGBA_SUITE_URL" "$MGBA_SUITE_REV"; then
    echo "source: mgba-emu/suite @ $MGBA_SUITE_REV (MIT)"
    if command -v arm-none-eabi-gcc >/dev/null 2>&1; then
        echo "mgba-emu/suite: SKIPPED (fetched at pinned revision; the"
        echo "  interactive text-grid menu needs key-driven navigation,"
        echo "  which this harness does not automate yet)"
    else
        echo "mgba-emu/suite: SKIPPED (build requires DEVKITARM;"
        echo "  arm-none-eabi-gcc is not on PATH). Install"
        echo "  https://devkitpro.org/wiki/Getting_Started and rerun to"
        echo "  build; automated running is still not implemented (the"
        echo "  suite navigates menus with KEYINPUT)."
    fi
else
    echo "SKIP: mgba-emu/suite is unavailable (offline or GitHub"
    echo "  unreachable). Reconnect and rerun, or point"
    echo "  OXYBOY_SUITE_CACHE at a populated cache directory."
fi

echo

echo "== regression corpus =="
if [ ! -f corpus.json ]; then
    echo "SKIP: corpus.json is missing at the repository root."
else
    if corpus_bin corpus --manifest corpus.json; then
        echo "corpus: PASS (determinism, pinned hashes, budgets)"
    else
        echo "corpus: FAILED (see the named entries above)" >&2
        exit 1
    fi
fi

echo "== validation report complete =="

# Threshold gate: every scored category must reach the
# minimum passed count in tests/suite-thresholds.txt. Categories that
# did not run (suite unavailable, ROM missing) are not gated; a
# timeout or runner failure counts as zero passed.
if [ "$SUITE_RAN" = 1 ]; then
    echo
    echo "== suite threshold gate =="
    thresholds=tests/suite-thresholds.txt
    if [ ! -f "$thresholds" ]; then
        echo "suite thresholds: FAILED ($thresholds is missing)" >&2
        exit 1
    fi
    gate_failures=0
    for cat in $CATEGORIES save_none save_sram save_flash64 save_flash128; do
        eval "ran=\${${cat}_ran:-0}"
        [ "$ran" = 1 ] || continue
        eval "passed=\${${cat}_passed:-0}"
        min=$(awk -v want="$cat" '$1 == want && $2 ~ /^[0-9]+$/ { print $2 }' "$thresholds")
        if [ -z "$min" ]; then
            echo "$cat: FAILED (no threshold line in $thresholds)" >&2
            gate_failures=$((gate_failures + 1))
        elif [ "$passed" -lt "$min" ]; then
            echo "$cat: FAILED (passed $passed, threshold $min)" >&2
            gate_failures=$((gate_failures + 1))
        else
            echo "$cat: ok (passed $passed, threshold $min)"
        fi
    done
    if [ "$gate_failures" -gt 0 ]; then
        echo "suite thresholds: FAILED ($gate_failures category/categories below threshold)" >&2
        exit 1
    fi
    echo "suite thresholds: PASS"
fi

exit 0
