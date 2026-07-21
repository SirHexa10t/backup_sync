#!/usr/bin/env bash
# One full ROUND of the move-vs-copy benchmark, hands-off:
#   rebuild (release) -> detect arm -> set up / reset corpus state -> cool caches ->
#   timed diff -> cool again -> timed sync -> sanity-check counts
# Everything is appended (never truncated) to $RESULTS via tee, so progress is visible live.
#
# You run this once per round, toggling the experiment patch between rounds:
#     ./run_move_bench.sh      # arm auto-detected: THRESHOLD (patch applied)
#     git stash                # (in the project dir)
#     ./run_move_bench.sh      # arm auto-detected: BASELINE
#     git stash pop
#     ./run_move_bench.sh      # THRESHOLD again
#     git stash                # …and so on, interleaved
#
# First run generates the corpus (BENCH_COUNT/BENCH_MIN_BYTES/BENCH_MAX_BYTES env vars are passed
# through to make_move_bench.sh); later runs reuse it and only reset the destination's state.
set -euo pipefail

# ── config: edit these (or export them as environment variables) ────────────────────────────────
MAKE_BENCH="${MAKE_BENCH:-./make_move_bench.sh}"   # path to the corpus/cool helper script
PROJECT_DIR="${PROJECT_DIR:-}"                     # the filesync repo (rebuilt every round)
SRC_DIR="${SRC_DIR:-}"                             # dir on the SOURCE drive; holds corpus/ only
DST_DIR="${DST_DIR:-}"                             # dir on the DEST drive; holds the twin only
RESULTS="${RESULTS:-./move_bench_results.log}"     # appended, never truncated
REPORT_DIR="${REPORT_DIR:-$HOME/mvbench_out}"      # filesync's own report files (outside drives)
# ─────────────────────────────────────────────────────────────────────────────────────────────────

die() { echo "error: $*" >&2; exit 1; }

[[ -n "$PROJECT_DIR" && -n "$SRC_DIR" && -n "$DST_DIR" ]] \
    || die "edit the config block first (PROJECT_DIR, SRC_DIR, DST_DIR)"
[[ -x "$MAKE_BENCH" ]] || die "helper script not found/executable: $MAKE_BENCH"
[[ -f "$PROJECT_DIR/Cargo.toml" ]] || die "not the filesync project: $PROJECT_DIR"
BIN="$PROJECT_DIR/target/release/filesync"

# Timed run: wall-clock measured by us (stable, machine-greppable line); stdout captured so the
# caller can sanity-check counts, then echoed into the log; stderr streams live.
LAST_OUT=""
run_timed() {
    local label="$1"; shift
    echo "── $label  ($(date -Is))"
    LAST_OUT="$(mktemp)"
    local t0 t1 rc=0
    t0=$(date +%s.%N)
    "$@" > "$LAST_OUT" || rc=$?
    t1=$(date +%s.%N)
    awk -v a="$t0" -v b="$t1" -v l="$label" -v r="$rc" \
        'BEGIN { printf "TIMING  %-18s  elapsed %8.2fs   (exit %d)\n", l, b - a, r }'
    cat "$LAST_OUT"
}

main() {
    echo
    echo "════════════════════════ ROUND START  $(date -Is) ════════════════════════"

    # 1) rebuild, then let the SOURCE say which arm this is — the log can't get mislabeled
    echo "── rebuild (release) ──"
    (cd "$PROJECT_DIR" && cargo build --release 2>&1 | tail -2)
    [[ -x "$BIN" ]] || die "no release binary at $BIN"
    local arm="BASELINE (move-detection active)"
    grep -q "MOVE_MIN_SIZE_EXPERIMENT" "$PROJECT_DIR/src/diff.rs" \
        && arm="THRESHOLD (small-file moves disabled — experiment patch applied)"
    local rev
    rev="$(git -C "$PROJECT_DIR" rev-parse --short HEAD 2>/dev/null || echo '?')"
    echo "ARM: $arm"
    echo "git: $rev$(git -C "$PROJECT_DIR" diff --quiet 2>/dev/null || echo ' + uncommitted changes')"

    # 2) corpus setup / reset. Layout: SRC_DIR/corpus  vs  DST_DIR/corpus_old (same files, other
    #    path) — every file pairs as a move. After a sync the destination holds corpus/; the reset
    #    is one rename back.
    mkdir -p "$SRC_DIR" "$DST_DIR" "$REPORT_DIR"
    local stray
    stray=$(find "$SRC_DIR" -mindepth 1 -maxdepth 1 ! -name corpus | head -3)
    [[ -z "$stray" ]] || die "SRC_DIR must hold only corpus/ — found: $stray"
    stray=$(find "$DST_DIR" -mindepth 1 -maxdepth 1 ! -name corpus ! -name corpus_old ! -name '.filesync.lock' | head -3)
    [[ -z "$stray" ]] || die "DST_DIR must hold only the twin — found: $stray (sync DELETES extras)"

    if [[ ! -d "$SRC_DIR/corpus" ]]; then
        echo "── first run: generating corpus ──"
        "$MAKE_BENCH" corpus "$SRC_DIR/corpus"
        echo "── copying corpus to the destination drive (one-time) ──"
        cp -a "$SRC_DIR/corpus" "$DST_DIR/corpus_old"
    fi
    if [[ -d "$DST_DIR/corpus" && -d "$DST_DIR/corpus_old" ]]; then
        die "both corpus/ and corpus_old/ exist at the destination — clean up by hand"
    elif [[ -d "$DST_DIR/corpus" ]]; then
        echo "── reset: renaming destination corpus/ back to corpus_old/ ──"
        mv "$DST_DIR/corpus" "$DST_DIR/corpus_old"
    fi
    [[ -d "$DST_DIR/corpus_old" ]] || die "destination twin missing"
    local count
    count=$(find "$SRC_DIR/corpus" -type f | wc -l)
    echo "corpus: $count files, $(du -sh "$SRC_DIR/corpus" | cut -f1) (src)"

    # 3) cool -> timed diff -> cool -> timed sync (the diff re-warms what it reads)
    echo "── cooling both trees ──"
    "$MAKE_BENCH" cool "$SRC_DIR/corpus"
    "$MAKE_BENCH" cool "$DST_DIR/corpus_old"

    run_timed "diff" \
        "$BIN" diff --from "$SRC_DIR" --to "$DST_DIR" --report "$REPORT_DIR" --unelevated

    echo "── cooling again (the diff re-warmed the caches) ──"
    "$MAKE_BENCH" cool "$SRC_DIR/corpus"
    "$MAKE_BENCH" cool "$DST_DIR/corpus_old"

    run_timed "sync" \
        "$BIN" sync --from "$SRC_DIR" --to "$DST_DIR" --report "$REPORT_DIR" --unelevated

    # 4) sanity: the counts must match the arm, or the round is mislabeled/diseased
    local moved copied
    moved=$(grep -oE 'moved:\s+[0-9]+' "$LAST_OUT" | grep -oE '[0-9]+' | head -1 || echo 0)
    copied=$(grep -oE 'copied:\s+[0-9]+' "$LAST_OUT" | grep -oE '[0-9]+' | head -1 || echo 0)
    if [[ "$arm" == BASELINE* ]]; then
        [[ "$moved" == "$count" && "$copied" == 0 ]] \
            && echo "SANITY: OK — all $count files moved (renames), none copied" \
            || echo "SANITY: *** MISMATCH *** expected moved=$count/copied=0, got moved=$moved/copied=$copied"
    else
        [[ "$copied" == "$count" && "$moved" == 0 ]] \
            && echo "SANITY: OK — all $count files copied (+deleted), none moved" \
            || echo "SANITY: *** MISMATCH *** expected copied=$count/moved=0, got moved=$moved/copied=$copied"
    fi
    echo "════════════════════════ ROUND END    $(date -Is) ════════════════════════"
}

main "$@" 2>&1 | tee -a "$RESULTS"
