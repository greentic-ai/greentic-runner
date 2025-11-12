#!/usr/bin/env bash
set -euo pipefail

: "${LOCAL_CHECK_ONLINE:=1}"
: "${CI:=1}"
: "${RUN_HOST:=never}"

echo "==> Local CI mirror (greentic-runner)"
export CARGO_TERM_COLOR=always
export RUSTFLAGS="-Dwarnings"
export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse

ONLINE=${LOCAL_CHECK_ONLINE:-0}
if [[ "$ONLINE" -eq 0 ]]; then
  export CARGO_NET_OFFLINE=true
else
  unset CARGO_NET_OFFLINE
fi

if [[ "${CI:-}" == "1" ]]; then
  set -x
fi

echo "==> cargo fmt --check"
cargo fmt --all --check

echo "==> cargo clippy (all targets, all features)"
cargo clippy --all-targets --all-features -- -D warnings

RUN_HOST="${RUN_HOST:-never}"
if [[ "${RUN_HOST}" == "always" ]] || { [[ "${RUN_HOST}" == "auto" ]] && [[ -f "./examples/index.json" ]]; }; then
  echo "==> Running host smoke (with safe defaults)"
  export PACK_INDEX_URL="${PACK_INDEX_URL:-./examples/index.json}"
  export PACK_CACHE_DIR="${PACK_CACHE_DIR:-.packs}"
  export DEFAULT_TENANT="${DEFAULT_TENANT:-demo}"

  if ! cargo run -p greentic-runner -- --bindings examples/bindings/demo.yaml --port 0 --once; then
    echo "Host smoke exited non-zero; continuing with tests but failing at end"
    HOST_SMOKE_FAILED=1
  fi
else
  echo "==> Skipping host smoke (no examples/index.json and RUN_HOST != always)"
fi

echo "==> crate tests"
cargo test -p greentic-runner

echo "==> workspace tests"
cargo test --workspace --all-targets --all-features

if [[ "${LOCAL_CHECK_PACKAGE:-1}" == "1" ]]; then
  echo "==> package dry-run (serialized)"
  if ! command -v jq >/dev/null 2>&1; then
    echo "jq not found; skipping package dry-run"
  else
    mapfile -t manifests < <(cargo metadata --no-deps --format-version=1 | jq -r '.packages[] | select(.publish != false and .publish != []) | .manifest_path')
    skipped_package=0
    for manifest in "${manifests[@]}"; do
      crate_dir="$(dirname "$manifest")"
      pushd "$crate_dir" >/dev/null
      if ! cargo package --no-verify --allow-dirty --quiet; then
        if [[ "${CARGO_NET_OFFLINE:-}" == "true" ]]; then
          echo "cargo package failed for $crate_dir while offline; skipping remaining packages"
          skipped_package=1
          popd >/dev/null
          break
        fi
        echo "package failed for $crate_dir"
        popd >/dev/null
        exit 1
      fi
      popd >/dev/null
    done
    if [[ "$skipped_package" -eq 1 ]]; then
      echo "Package dry-run unfinished due to offline mode; rerun with LOCAL_CHECK_ONLINE=1 to verify packaging"
    fi
  fi
fi

if [[ "${HOST_SMOKE_FAILED:-0}" == "1" ]]; then
  echo "Host smoke failed (see log above)"
  exit 1
fi

echo "==> OK"
