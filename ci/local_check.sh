#!/usr/bin/env bash
set -euo pipefail

: "${CI:=0}"
: "${RUN_HOST:=never}"

RUST_TOOLCHAIN_VERSION="1.95.0"
echo "==> Local CI mirror (greentic-runner, rustc ${RUST_TOOLCHAIN_VERSION})"
export CARGO_TERM_COLOR=always
export RUSTFLAGS="-Dwarnings"
export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
export GREENTIC_PROVIDER_CORE_ONLY="${GREENTIC_PROVIDER_CORE_ONLY:-1}"

if [[ -z "${RUSTUP_TOOLCHAIN:-}" ]]; then
  export RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN_VERSION"
elif [[ "$RUSTUP_TOOLCHAIN" != "${RUST_TOOLCHAIN_VERSION}"* ]]; then
  echo "warning: RUSTUP_TOOLCHAIN=$RUSTUP_TOOLCHAIN differs from expected $RUST_TOOLCHAIN_VERSION" >&2
fi

# If you *really* want to prefetch in CI, do it unconditionally here:
# echo "==> Prefetching dependencies (cargo fetch --locked)"
# cargo fetch --locked

if [[ "${CI:-}" == "1" || "${CI:-}" == "true" ]]; then
  set -x
fi

if [[ -z "${LOCAL_CHECK_PACKAGE+x}" ]]; then
  if [[ "${CI:-}" == "1" || "${CI:-}" == "true" ]]; then
    LOCAL_CHECK_PACKAGE=0
  else
    LOCAL_CHECK_PACKAGE=1
  fi
fi

run_fmt() {
  echo "==> cargo fmt --check"
  cargo fmt --all --check
}

run_clippy() {
  echo "==> cargo clippy (all targets, all features)"
  cargo clippy --all-targets --all-features -- -D warnings
}

run_host_smoke() {
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
}

run_crate_tests() {
  echo "==> crate tests"
  cargo test -p greentic-runner
}

run_workspace_tests() {
  echo "==> workspace tests"
  cargo test --workspace --all-targets --all-features
}

run_conformance() {
  if [[ "${RUN_CONFORMANCE:-0}" == "1" ]]; then
    echo "==> conformance harness"
    cargo run -p greentic-runner -- conformance --packs tests/fixtures/packs --level L1
  else
    echo "==> Skipping conformance (RUN_CONFORMANCE != 1)"
  fi
}

run_package() {
  if [[ "${LOCAL_CHECK_PACKAGE}" == "1" ]]; then
    echo "==> package dry-run (serialized)"
    if ! command -v jq >/dev/null 2>&1; then
      echo "jq not found; skipping package dry-run"
    else
      manifests=$(cargo metadata --no-deps --format-version=1 | jq -r '.packages[] | select(.publish != false and .publish != []) | .manifest_path')
      skipped_package=0
      while IFS= read -r manifest; do
        [[ -z "$manifest" ]] && continue
        crate_dir="$(dirname "$manifest")"
        pushd "$crate_dir" >/dev/null
        if ! cargo package --no-verify --allow-dirty --quiet; then
          echo "package failed for $crate_dir"
          popd >/dev/null
          exit 1
        fi
        popd >/dev/null
      done <<< "$manifests"
      if [[ "$skipped_package" -eq 1 ]]; then
        echo "Package dry-run unfinished due to offline mode; rerun with LOCAL_CHECK_ONLINE=1 to verify packaging"
      fi
    fi
  fi
}

default_steps=("fmt" "clippy" "host_smoke" "crate_tests" "workspace_tests" "conformance" "package")
if [[ -n "${LOCAL_CHECK_STEPS:-}" ]]; then
  steps_list="${LOCAL_CHECK_STEPS//,/ }"
  read -r -a steps <<< "$steps_list"
else
  steps=("${default_steps[@]}")
fi

for step in "${steps[@]}"; do
  case "$step" in
    fmt) run_fmt ;;
    clippy) run_clippy ;;
    host_smoke) run_host_smoke ;;
    crate_tests) run_crate_tests ;;
    workspace_tests) run_workspace_tests ;;
    conformance) run_conformance ;;
    package) run_package ;;
    *)
      echo "Unknown LOCAL_CHECK_STEPS entry: $step"
      exit 1
      ;;
  esac
done

if [[ "${HOST_SMOKE_FAILED:-0}" == "1" ]]; then
  echo "Host smoke failed (see log above)"
  exit 1
fi

echo "==> OK"
