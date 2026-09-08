#!/bin/sh
set -eu

max_gib="${DISTILLPDF_TARGET_MAX_GIB:-80}"
min_free_gib="${DISTILLPDF_MIN_FREE_GIB:-40}"
target_input="${CARGO_TARGET_DIR:-target}"
execute=false

case "$#:${1:-}" in
  0:) ;;
  1:--execute) execute=true ;;
  *) echo "usage: scripts/prune-target.sh [--execute]" >&2; exit 2 ;;
esac
for value in "$max_gib" "$min_free_gib"; do
  case "$value" in
    ''|*[!0-9]*) echo "prune thresholds must be non-negative integers" >&2; exit 2 ;;
  esac
done

if [ ! -e "$target_input" ]; then
  echo "target prune: no-op (target does not exist: $target_input)"
  exit 0
fi
if [ ! -d "$target_input" ]; then
  echo "target prune: target is not a directory: $target_input" >&2
  exit 2
fi
target_dir=$(cd "$target_input" && pwd -P)
repo_dir=$(pwd -P)
case "$target_dir" in
  /|"$repo_dir") echo "target prune: refusing broad target: $target_dir" >&2; exit 2 ;;
esac
case "$repo_dir" in
  "$target_dir"/*) echo "target prune: refusing target that contains the repository: $target_dir" >&2; exit 2 ;;
esac
cache_tag="$target_dir/CACHEDIR.TAG"
if [ ! -f "$cache_tag" ] || ! grep -q '^Signature: 8a477f597d28d172789f06886806bc55$' "$cache_tag"; then
  echo "target prune: refusing unmarked cache directory: $target_dir" >&2
  exit 2
fi

if du -skA "$target_dir" >/dev/null 2>&1; then
  target_kib=$(du -skA "$target_dir" | awk 'NR == 1 { print $1 }')
elif du -sk --apparent-size "$target_dir" >/dev/null 2>&1; then
  target_kib=$(du -sk --apparent-size "$target_dir" | awk 'NR == 1 { print $1 }')
else
  echo "target prune: apparent-size probe unavailable" >&2
  exit 1
fi
free_kib=$(df -Pk "$target_dir" | awk 'NR == 2 { print $4 }')
case "$target_kib" in ''|*[!0-9]*) echo "target prune: invalid target-size output" >&2; exit 1 ;; esac
case "$free_kib" in ''|*[!0-9]*) echo "target prune: invalid free-space output" >&2; exit 1 ;; esac
max_kib=$((max_gib * 1024 * 1024))
min_free_kib=$((min_free_gib * 1024 * 1024))

if [ "$target_kib" -le "$max_kib" ] && [ "$free_kib" -ge "$min_free_kib" ]; then
  echo "target prune: no-op (${target_kib} KiB target; ${free_kib} KiB free)"
  exit 0
fi
if [ "$execute" != true ]; then
  echo "target prune: needed but not run; re-run with --execute" >&2
  exit 3
fi
if ! command -v lsof >/dev/null 2>&1; then
  echo "target prune: refused; lsof is required to establish that the target is idle" >&2
  exit 4
fi
lsof_output=$(mktemp "${TMPDIR:-/tmp}/distillpdf-prune.XXXXXX")
trap 'rm -f "$lsof_output"' EXIT HUP INT TERM
if lsof +D "$target_dir" >"$lsof_output" 2>&1; then
  if [ -s "$lsof_output" ]; then
    echo "target prune: refused because $target_dir is in use" >&2
    exit 4
  fi
else
  status=$?
  if [ "$status" -ne 1 ] || [ -s "$lsof_output" ]; then
    echo "target prune: refused because the in-use probe failed" >&2
    exit 4
  fi
fi
cargo clean --target-dir "$target_dir"
