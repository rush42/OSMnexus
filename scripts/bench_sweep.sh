#!/usr/bin/env bash
#
# Phase 0 harness for the output redesign (see `output_master_plan.md` §4).
#
# Runs a {backend} × {threads} matrix against one PBF extract and writes a TSV of per-phase
# timings and peak RSS. Every gate in the master plan is decided off this script's output, so it
# is deliberately strict about the things that have already produced one wrong conclusion in this
# project's history (`output_plan.md`: a benchmark taken under unnoticed background load):
#
#   * refuses to run on a loaded machine unless --force
#   * re-warms the page cache identically before every single run
#   * records the binary's commit hash + dirty state in the output
#   * repeats each cell and keeps every rep (no averaging away variance — low thread counts have
#     measurably more of it than high ones)
#
# Phases parsed out of `RUST_LOG=info` output:
#   blob   blob index build
#   passA  Pass A (classify ways + emit tags)
#   passB  Pass B (collect node coords)
#   select Select phase   (encloses passA + passB)
#   mat    Materialize phase
#   join   post-run join/write step, derived: total - blob - select - mat
#
# Usage:
#   scripts/bench_sweep.sh --pbf data/germany-latest.osm.pbf
#   scripts/bench_sweep.sh --pbf x.osm.pbf --threads "1 8 32" --backends "csv parquet" --reps 3
#
# `pg` is intentionally NOT in the default backend list: it needs a live database, and its
# ordered/unordered A/B is driven by OSMNEXUS_FORCE_ORDERED rather than by this matrix.

set -euo pipefail

PBF=""
THREADS="1 2 4 8 16 32 56"
BACKENDS="parquet"
REPS=2
OUTDIR=""
CONFIG="configs/tilda"
BIN=""
WORK=""
FORCE=0
LOAD_MAX=2.0

die() { echo "error: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --pbf)      PBF=$2; shift 2 ;;
    --threads)  THREADS=$2; shift 2 ;;
    --backends) BACKENDS=$2; shift 2 ;;
    --reps)     REPS=$2; shift 2 ;;
    --out)      OUTDIR=$2; shift 2 ;;
    --config)   CONFIG=$2; shift 2 ;;
    --bin)      BIN=$2; shift 2 ;;
    --work)     WORK=$2; shift 2 ;;
    --force)    FORCE=1; shift ;;
    -h|--help)  sed -n '2,30p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

REPO=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$REPO"

[ -n "$PBF" ] || die "--pbf is required"
[ -f "$PBF" ] || die "no such file: $PBF"
PBF=$(readlink -f "$PBF")

BIN=${BIN:-$REPO/target/release/osmnexus}
[ -x "$BIN" ] || die "no release binary at $BIN (cargo build --release --bin osmnexus)"

# --- provenance -------------------------------------------------------------
COMMIT=$(git rev-parse --short HEAD)
DIRTY=$(git diff --quiet && git diff --cached --quiet && echo clean || echo DIRTY)
NPROC=$(nproc)
HOST=$(hostname)
STAMP=$(date +%Y%m%d-%H%M%S)

OUTDIR=${OUTDIR:-$REPO/bench-results/$STAMP-$COMMIT}
mkdir -p "$OUTDIR"

# --- quiet-machine check ----------------------------------------------------
# The one guard that matters most: `output_plan.md` records a wrong ordered-vs-unordered
# conclusion caused by measuring under varying background load.
LOAD=$(awk '{print $1}' /proc/loadavg)
if [ "$FORCE" -eq 0 ] && awk -v l="$LOAD" -v m="$LOAD_MAX" 'BEGIN{exit !(l>m)}'; then
  die "1-min load average is $LOAD (> $LOAD_MAX). Benchmark on an idle machine, or pass --force."
fi
if [ "$FORCE" -eq 0 ] && [ -n "${SLURM_JOB_ID:-}" ] && [ "${SLURM_CPUS_ON_NODE:-0}" -lt 8 ]; then
  die "Slurm allocation has only ${SLURM_CPUS_ON_NODE} CPU(s) — a thread sweep needs a real allocation."
fi

# --- staging ----------------------------------------------------------------
# Prefer node-local disk so the matrix measures the pipeline, not a network filesystem.
WORK=${WORK:-$(mktemp -d "${TMPDIR:-/tmp}/bench-sweep-XXXXXX")}
mkdir -p "$WORK"
STAGED=$WORK/input.osm.pbf
cp "$PBF" "$STAGED"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

SUMMARY=$OUTDIR/summary.tsv
printf 'backend\tthreads\trep\twall_s\ttotal_s\tblob_s\tpassA_s\tpassB_s\tselect_s\tmat_s\tjoin_s\tpeak_rss_kb\texit\n' > "$SUMMARY"

cat > "$OUTDIR/meta.txt" <<EOF
commit     $COMMIT ($DIRTY)
host       $HOST
nproc      $NPROC
pbf        $PBF ($(stat -c %s "$PBF") bytes)
config     $CONFIG
backends   $BACKENDS
threads    $THREADS
reps       $REPS
started    $(date -Is)
loadavg    $LOAD
EOF
echo "=== bench sweep → $OUTDIR ==="
cat "$OUTDIR/meta.txt"

num() { grep -oP "$1" "$2" 2>/dev/null | tail -1 || true; }

for REP in $(seq 1 "$REPS"); do
  for BACKEND in $BACKENDS; do
    for T in $THREADS; do
      if [ "$T" -gt "$NPROC" ]; then
        echo "skip threads=$T (> nproc=$NPROC)"
        continue
      fi
      OUT=$WORK/out; rm -rf "$OUT"; mkdir -p "$OUT"
      TAG=$BACKEND-t$T-r$REP
      LOG=$OUTDIR/run-$TAG.log
      TIMEF=$OUTDIR/time-$TAG.txt

      cat "$STAGED" > /dev/null   # identical warm page cache for every run

      echo "--- $TAG  $(date -Is) ---"
      START=$(date +%s.%N)
      set +e
      /usr/bin/time -v -o "$TIMEF" \
        env RUST_LOG=info "$BIN" "$STAGED" \
          --config-dir "$CONFIG" --threads "$T" \
          --output "$BACKEND" --out-dir "$OUT" \
        > "$LOG" 2>&1
      RC=$?
      set -e
      WALL=$(echo "$(date +%s.%N) - $START" | bc)

      # strip ANSI so the greps below match regardless of tracing's colour choice
      sed -i 's/\x1b\[[0-9;]*m//g' "$LOG"

      BLOB=$(num 'blob index build: \K[0-9.]+' "$LOG")
      PA=$(num 'Pass A \(classify ways \+ emit tags\): \K[0-9.]+' "$LOG")
      PB=$(num 'Pass B \(collect node coords\): \K[0-9.]+' "$LOG")
      SEL=$(num 'Select phase time: \K[0-9.]+' "$LOG")
      MAT=$(num 'Materialize phase time: \K[0-9.]+' "$LOG")
      TOT=$(num 'Done\. Total: \K[0-9.]+' "$LOG")
      RSS=$(awk '/Maximum resident set size/{print $NF}' "$TIMEF" 2>/dev/null || echo NA)

      JOIN=NA
      if [ -n "$TOT" ] && [ -n "$SEL" ] && [ -n "$MAT" ] && [ -n "$BLOB" ]; then
        # via awk, not bare `bc`, so the column reads 0.2 rather than bc's leading-dot .2
        JOIN=$(echo "$TOT - $BLOB - $SEL - $MAT" | bc | awk '{printf "%.1f", $0}')
      fi

      printf '%s\t%s\t%s\t%.1f\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$BACKEND" "$T" "$REP" "$WALL" "${TOT:-NA}" "${BLOB:-NA}" "${PA:-NA}" "${PB:-NA}" \
        "${SEL:-NA}" "${MAT:-NA}" "$JOIN" "$RSS" "$RC" >> "$SUMMARY"

      echo "$TAG total=${TOT:-NA}s select=${SEL:-NA}s mat=${MAT:-NA}s join=${JOIN}s rss=${RSS}kB rc=$RC"
      [ "$RC" -eq 0 ] || echo "  !! non-zero exit — see $LOG"
      rm -rf "$OUT"
    done
  done
done

echo
echo "=== $SUMMARY ==="
column -t "$SUMMARY"
