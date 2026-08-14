#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> gdal-alt-core unit + integration tests"
RUSTFLAGS="${RUSTFLAGS:--D warnings}" cargo test -p gdal-alt-core

echo "==> encode smoke (in-process fixtures)"
RUSTFLAGS="${RUSTFLAGS:--D warnings}" cargo test -p gdal-alt-core --test encode_smoke -- --nocapture

if [[ -x "$ROOT/target/release/fastcog" ]] && [[ -x "$ROOT/target/release/fastvalidate" ]]; then
  FIXTURE_DIR="${FIXTURE_DIR:-$ROOT/test_data}"
  if [[ -d "$FIXTURE_DIR" ]]; then
    echo "==> encode smoke (CLI on fixtures in $FIXTURE_DIR)"
    encoded=0
    for path in "$FIXTURE_DIR"/*.{tif,tiff,TIF,TIFF}; do
      [[ -f "$path" ]] || continue
      out="$(mktemp --suffix=.tif)"
      trap 'rm -f "$out"' EXIT
      "$ROOT/target/release/fastcog" "$path" "$out" -q
      "$ROOT/target/release/fastvalidate" "$out"
      rm -f "$out"
      trap - EXIT
      encoded=$((encoded + 1))
      if [[ "$encoded" -ge 3 ]]; then
        break
      fi
    done
    if [[ "$encoded" -eq 0 ]]; then
      echo "No raster fixtures found under $FIXTURE_DIR (skipped CLI smoke)"
    fi
  fi
fi

echo "encode smoke: OK"
