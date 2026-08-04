#!/usr/bin/env bash

# Build the one release wheel consumed by release-check.sh and print its exact path.
# The caller must provide a fresh, per-run output directory: sharing target/wheels
# lets a stale or concurrent build win an mtime-based selector after this build ends.
build_release_wheel() {
  if [ "$#" -ne 3 ]; then
    echo "usage: build_release_wheel <maturin> <build-python> <empty-output-dir>" >&2
    return 2
  fi

  local maturin="$1"
  local build_python="$2"
  local output_dir="$3"
  local existing
  local wheels

  if [ ! -d "$output_dir" ]; then
    echo "FAILED — wheel output directory does not exist: $output_dir" >&2
    return 2
  fi

  shopt -s nullglob
  existing=("$output_dir"/*)
  shopt -u nullglob
  if [ "${#existing[@]}" -ne 0 ]; then
    echo "FAILED — wheel output directory is not empty: $output_dir" >&2
    return 2
  fi

  # Keep this release-shaped and pinned to BUILD_PY. Send maturin's complete log to
  # stderr so command substitution captures only the verified artifact path below.
  "$maturin" build --release --features extension-module -i "$build_python" \
    --out "$output_dir" >&2

  shopt -s nullglob
  wheels=("$output_dir"/distillpdf-*-abi3-*.whl)
  shopt -u nullglob
  if [ "${#wheels[@]}" -ne 1 ]; then
    echo "FAILED — maturin produced ${#wheels[@]} matching distillpdf abi3 wheels in $output_dir; expected exactly one." >&2
    return 2
  fi

  printf '%s\n' "${wheels[0]}"
}
