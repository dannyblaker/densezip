#!/usr/bin/env bash
# Benchmark densezip against gzip, bzip2, xz, zstd, 7z on every file in a
# corpus directory. Emits a markdown table and verifies densezip roundtrips.
#
# Usage: bench/bench.sh <corpus-dir> [--no-cm] [extra densezip args...]
set -euo pipefail

CORPUS="${1:?usage: bench.sh <corpus-dir> [--no-cm]}"
shift || true
BZ_ARGS=("$@")
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BZ="$ROOT/target/release/dnz"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

command -v 7z >/dev/null || { echo "7z not found"; exit 1; }
[ -x "$BZ" ] || { echo "build first: cargo build --release"; exit 1; }

size() { stat -c%s "$1"; }

printf "| %-14s | %10s | %10s | %10s | %10s | %10s | %10s | %10s | %7s |\n" \
  "file" "original" "gzip-9" "bzip2-9" "xz-9e" "zstd-22" "7z-mx9" "densezip" "vs best"
printf "|%s|%s|%s|%s|%s|%s|%s|%s|%s|\n" "---" "---:" "---:" "---:" "---:" "---:" "---:" "---:" "---:"

tot_orig=0; tot_best=0; tot_bz=0
for f in "$CORPUS"/*; do
  [ -f "$f" ] || continue
  name="$(basename "$f")"
  orig=$(size "$f")

  gz=$(gzip -9 -c "$f" | wc -c)
  bz2=$(bzip2 -9 -c "$f" | wc -c)
  xz_=$(xz -9e -T1 -c "$f" | wc -c)
  zst=$(zstd -q --ultra -22 --long=27 -c "$f" | wc -c)
  rm -f "$WORK/a.7z"; 7z a -t7z -mx=9 -bso0 -bsp0 "$WORK/a.7z" "$f" >/dev/null
  sz7=$(size "$WORK/a.7z")

  rm -f "$WORK/a.dnz"
  "$BZ" a "$WORK/a.dnz" "$f" "${BZ_ARGS[@]}" >/dev/null
  bzs=$(size "$WORK/a.dnz")
  "$BZ" t "$WORK/a.dnz" >/dev/null   # verify every benchmark archive

  best=$(printf "%s\n" "$gz" "$bz2" "$xz_" "$zst" "$sz7" | sort -n | head -1)
  margin=$(awk "BEGIN{printf \"%+.1f%%\", ($bzs-$best)*100.0/$best}")
  printf "| %-14s | %10d | %10d | %10d | %10d | %10d | %10d | %10d | %7s |\n" \
    "$name" "$orig" "$gz" "$bz2" "$xz_" "$zst" "$sz7" "$bzs" "$margin"
  tot_orig=$((tot_orig+orig)); tot_best=$((tot_best+best)); tot_bz=$((tot_bz+bzs))
done

margin=$(awk "BEGIN{printf \"%+.1f%%\", ($tot_bz-$tot_best)*100.0/$tot_best}")
printf "| %-14s | %10d | %10s | %10s | %10s | %10s | %10s | %10d | %7s |\n" \
  "**TOTAL**" "$tot_orig" "" "" "" "" "(best-of)$tot_best" "$tot_bz" "$margin"
