#!/usr/bin/env bash
set -euo pipefail
mkdir -p dist
packc build --in . --out dist/pack.wasm --manifest dist/manifest.cbor --sbom dist/sbom.cdx.json
echo "Built dist/pack.wasm"
