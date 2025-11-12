#!/usr/bin/env bash
# Usage:
#   LOCAL_CHECK_ONLINE=1 LOCAL_CHECK_STRICT=1 CI=1 ci/local_check.sh
# Defaults: offline, non-strict, local.

set -euo pipefail

if [ "${LOCAL_CHECK_VERBOSE:-0}" != "0" ]; then
  set -x
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

STRICT=${LOCAL_CHECK_STRICT:-0}
ONLINE=${LOCAL_CHECK_ONLINE:-0}
CI_MODE=${CI:-0}
STATUS=0

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "[miss] $1" >&2
    return 1
  }
}

step() {
  echo ""
  echo "▶ $*"
}

run_step() {
  local desc="$1"
  shift
  step "$desc"
  if "$@"; then
    echo "[ok] $desc"
  else
    echo "[fail] $desc" >&2
    STATUS=1
  fi
}

skip_step() {
  echo "[skip] $1"
}

run_or_skip() {
  local desc="$1"
  shift
  if "$@"; then
    return 0
  fi
  skip_step "$desc"
}

ensure_core_tooling() {
  local missing=0
  for tool in rustc cargo python3; do
    if ! need "$tool"; then
      missing=1
      echo "[warn] missing required tool '$tool'"
    fi
  done
  if [ "$missing" -eq 1 ]; then
    if [ "$STRICT" -eq 1 ]; then
      echo "[fatal] required tooling missing" >&2
      exit 1
    else
      echo "[skip] core tooling unavailable; exiting" >&2
      exit 0
    fi
  fi
}

install_pre_push_hook() {
  local git_dir=".git"
  local hook="$git_dir/hooks/pre-push"
  if [ ! -d "$git_dir" ]; then
    return
  fi
  if [ -f "$hook" ]; then
    return
  fi
  if ! cat > "$hook" <<'HOOK'; then
#!/usr/bin/env bash
set -euo pipefail
ci/local_check.sh
HOOK
    echo "[warn] unable to install git pre-push hook (write failed)"
    return
  fi
  if ! chmod +x "$hook"; then
    echo "[warn] unable to mark git pre-push hook executable"
    return
  fi
  echo "[info] installed git pre-push hook -> ci/local_check.sh"
}

if [ "$ONLINE" -eq 0 ]; then
  export CARGO_NET_OFFLINE=true
else
  unset CARGO_NET_OFFLINE
fi

export CARGO_TERM_COLOR=always
export RUSTFLAGS="-Dwarnings"
export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse

RUST_TOOLCHAIN_FILE="$REPO_ROOT/rust-toolchain.toml"
if [ -f "$RUST_TOOLCHAIN_FILE" ]; then
  TOOLCHAIN_CHANNEL="$(python3 - "$RUST_TOOLCHAIN_FILE" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
match = re.search(r'channel\s*=\s*"([^"]+)"', text)
print(match.group(1) if match else "nightly")
PY
)"
else
  TOOLCHAIN_CHANNEL="nightly"
fi

export RUSTUP_TOOLCHAIN="$TOOLCHAIN_CHANNEL"

echo "[local-check] root: $REPO_ROOT"
echo "[local-check] STRICT=$STRICT ONLINE=$ONLINE VERBOSE=${LOCAL_CHECK_VERBOSE:-0} CI=$CI_MODE toolchain=$TOOLCHAIN_CHANNEL"

ensure_core_tooling

rustc --version
cargo --version

run_step "cargo fmt" cargo fmt --all -- --check
run_step "cargo clippy" cargo clippy --all-targets --all-features -- -D warnings
run_step "cargo run example run_demo" cargo run -p greentic-runner --example run_demo
run_step "cargo test -p greentic-runner" cargo test -p greentic-runner
run_step "cargo test --all-targets --all-features" cargo test --all-targets --all-features

package_publishable_crates() {
  local metadata_packages

  mapfile -t metadata_packages < <(
    cargo metadata --no-deps --format-version=1 |
    python3 - <<'PY'
import json, sys
text = sys.stdin.read()
start = text.find('{')
if start == -1:
    raise SystemExit("failed to locate cargo metadata JSON")
metadata = json.loads(text[start:])
for pkg in metadata["packages"]:
    publish = pkg.get("publish")
    if publish == []:
        continue
    print(f"{pkg['name']}|{pkg['manifest_path']}")
PY
  )

  local package_args=(--quiet)
  if [ "$CI_MODE" -eq 0 ]; then
    package_args=(--allow-dirty --no-verify --quiet)
  fi

  for entry in "${metadata_packages[@]}"; do
    local package_name="${entry%%|*}"
    local manifest_path="${entry#*|}"
    local crate_dir
    crate_dir="$(dirname "$manifest_path")"
    (
      cd "$crate_dir"
      cargo clean -p "$package_name" >/dev/null 2>&1 || true
      cargo package "${package_args[@]}"
    )
  done
}

run_step "cargo package dry-run for publishable crates" package_publishable_crates

install_pre_push_hook

if [ "$STATUS" -eq 0 ]; then
  printf '\n[local-check] success\n'
else
  printf '\n[local-check] completed with failures\n' >&2
fi

exit "$STATUS"
