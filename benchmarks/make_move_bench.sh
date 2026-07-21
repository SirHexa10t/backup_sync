#!/usr/bin/env bash
# Build corpora for the move-vs-copy benchmark: does move-detection (hash both sides + rename)
# beat plain copy+delete for many SMALL files, end to end, on real drives?
#
# Protocol (run `./make_move_bench.sh help` for the full runbook):
#   corpus  — create the canonical tree (goes to the SOURCE drive)
#   twin    — create the same tree with every file RENAMED (goes to the DESTINATION drive);
#             contents are byte-identical, so filesync pairs every file as a move
#   reset   — after a sync, rename the destination's files back to the pre-run state, so the
#             next timed run does full work again (without this, run 2 is a no-op scan)
#   dropcaches — flush the page cache (sudo) so timed runs read from the device, not RAM
#
# Tunables (env): BENCH_COUNT (default 100000 files), BENCH_MIN_BYTES (1024), BENCH_MAX_BYTES (8192).
# Content is /dev/urandom garbage — every file distinct, incompressible, undedupable.
set -euo pipefail

COUNT="${BENCH_COUNT:-100000}"
MIN="${BENCH_MIN_BYTES:-1024}"
MAX="${BENCH_MAX_BYTES:-8192}"
FILES_PER_DIR=1000

die() { echo "error: $*" >&2; exit 1; }

cmd_corpus() {
    local dir="${1:?usage: $0 corpus <new-dir>}"
    [[ -e "$dir" ]] && die "refusing to touch an existing path: $dir"
    mkdir -p "$dir"
    echo "creating $COUNT files of $MIN..$MAX random bytes under $dir ..."
    local subdirs=$(( (COUNT + FILES_PER_DIR - 1) / FILES_PER_DIR ))
    for ((s = 0; s < subdirs; s++)); do mkdir -p "$dir/d$(printf '%04d' "$s")"; done
    seq -w 1 "$COUNT" | xargs -P "$(nproc)" -I{} bash -c '
        dir="$1"; i="$2"; min="$3"; max="$4"; per_dir="$5"
        sub=$(printf "d%04d" $(( (10#$i - 1) / per_dir )))   # files 1..N/dir -> d0000, ...
        size=$(( min + RANDOM % (max - min + 1) ))
        head -c "$size" /dev/urandom > "$dir/$sub/f_$i.bin"
    ' _ "$dir" {} "$MIN" "$MAX" "$FILES_PER_DIR"
    echo "done: $(find "$dir" -type f | wc -l) files, $(du -sh "$dir" | cut -f1)"
}

cmd_twin() {
    local corpus="${1:?usage: $0 twin <corpus-dir> <new-twin-dir>}"
    local dir="${2:?usage: $0 twin <corpus-dir> <new-twin-dir>}"
    [[ -d "$corpus" ]] || die "corpus not found: $corpus"
    [[ -e "$dir" ]] && die "refusing to touch an existing path: $dir"
    mkdir -p "$dir"
    echo "building renamed twin of $corpus at $dir ..."
    (cd "$corpus" && find . -type d) | (cd "$dir" && xargs mkdir -p)
    (cd "$corpus" && find . -type f -name 'f_*.bin' -printf '%P\n') \
        | xargs -P "$(nproc)" -I{} bash -c '
            corpus="$1"; dir="$2"; rel="$3"
            d=$(dirname "$rel"); b=$(basename "$rel")
            cp "$corpus/$rel" "$dir/$d/renamed_$b"
        ' _ "$corpus" "$dir" {}
    echo "done: $(find "$dir" -type f | wc -l) files (every name prefixed 'renamed_')"
}

cmd_reset() {
    local dir="${1:?usage: $0 reset <twin-dir>}"
    [[ -d "$dir" ]] || die "twin not found: $dir"
    # After a sync the destination carries the canonical names (f_*.bin) — whether they got there
    # by rename (baseline) or by copy+delete (threshold build) — so the reset is the same either
    # way: put the 'renamed_' prefix back. Metadata-only; takes seconds.
    local before
    before=$(cd "$dir" && find . -type f -name 'f_*.bin' | wc -l)
    [[ "$before" -gt 0 ]] || die "nothing to reset (no canonical f_*.bin files) — did the sync run?"
    (cd "$dir" && find . -type f -name 'f_*.bin' -printf '%P\n') \
        | xargs -P "$(nproc)" -I{} bash -c '
            dir="$1"; rel="$2"; d=$(dirname "$rel"); b=$(basename "$rel")
            mv "$dir/$rel" "$dir/$d/renamed_$b"
        ' _ "$dir" {}
    echo "reset: $before files renamed back — twin is in its pre-run state again"
}

cmd_dropcaches() {
    sync
    echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
    echo "page cache dropped — the next run reads from the devices"
}

# Targeted alternative to dropcaches: evict ONLY the given tree's file content from RAM, leaving
# the rest of the system's cache (your session) untouched. No root needed. Uses dd's `nocache`
# (fadvise DONTNEED — the same kernel mechanism filesync's verify stage uses). Run it on BOTH the
# source corpus and the destination twin before each timed run. Note: file metadata (dentries/
# inodes) stays warm — acceptable, and identical for both arms of the A/B.
cmd_cool() {
    local dir="${1:?usage: $0 cool <dir>}"
    [[ -d "$dir" ]] || die "not a directory: $dir"
    sync
    local n
    n=$(find "$dir" -type f | wc -l)
    find "$dir" -type f -print0 \
        | xargs -0 -P "$(nproc)" -I{} dd if={} iflag=nocache count=0 status=none
    echo "cooled: content cache dropped for $n files under $dir (rest of the system untouched)"
}

cmd_help() {
    cat <<'EOF'
Runbook — move-vs-copy benchmark (baseline first, then the threshold build):

  1.  ./make_move_bench.sh corpus /somewhere/local/mvbench_corpus
  2.  cp -a /somewhere/local/mvbench_corpus  <SOURCE-DRIVE>/mvbench_src
  3.  ./make_move_bench.sh twin /somewhere/local/mvbench_corpus  <DEST-DRIVE>/mvbench_dst
      # SAFETY: mvbench_dst must be a DEDICATED directory — sync mirrors, i.e. deletes extras.
  4.  For EACH timed run, first evict the corpus content from RAM — one of:
        ./make_move_bench.sh cool <SOURCE-DRIVE>/mvbench_src     # targeted, no root, session
        ./make_move_bench.sh cool <DEST-DRIVE>/mvbench_dst       #   caches untouched
      or: unmount+remount the two drives (exact per-device eviction)
      or: ./make_move_bench.sh dropcaches                        # system-WIDE (needs sudo;
                                                                 #   safe, but briefly slows
                                                                 #   everything as caches refill)
        /usr/bin/time -v  filesync sync \
            --from <SOURCE-DRIVE>/mvbench_src  --to <DEST-DRIVE>/mvbench_dst \
            --report ~/mvbench_out  --unelevated  2> runN.log
        # --unelevated: skips the sudo prompt (don't time a password entry!)
        # stderr is not a terminal -> runN.log gets per-phase lines ("scanned ... in Xs" etc.)
        # sanity check the findings: baseline runs must show moved == file count, copied 0
  5.  Between runs:  ./make_move_bench.sh reset <DEST-DRIVE>/mvbench_dst
  6.  Interleave configurations to spread drive/thermal drift:
        baseline, threshold, baseline, threshold   (not baseline x2 then threshold x2)
EOF
}

case "${1:-help}" in
    corpus)     shift; cmd_corpus "$@" ;;
    twin)       shift; cmd_twin "$@" ;;
    reset)      shift; cmd_reset "$@" ;;
    cool)       shift; cmd_cool "$@" ;;
    dropcaches) shift; cmd_dropcaches "$@" ;;
    help|*)     cmd_help ;;
esac
