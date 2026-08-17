#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo run -p gdal-alt-core --bin footprint-fixture-gen -- "${1:-test_data/footprint}"
