#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCALES_FILE="$ROOT_DIR/assets/i18n/locales.json"
SOURCE_FILE="$ROOT_DIR/assets/i18n/en.json"

log() {
  printf '[i18n] %s\n' "$*"
}

fail() {
  printf '[i18n] error: %s\n' "$*" >&2
  exit 1
}

ensure_codex() {
  if command -v codex >/dev/null 2>&1 && codex --version >/dev/null 2>&1; then
    return
  fi
  log "Codex CLI missing or broken; attempting install"
  if command -v npm >/dev/null 2>&1; then
    log "installing Codex CLI via npm"
    npm i -g @openai/codex@latest || fail "failed to install Codex CLI via npm"
  elif command -v brew >/dev/null 2>&1; then
    log "installing Codex CLI via brew"
    brew install codex || fail "failed to install Codex CLI via brew"
  else
    fail "Codex CLI not found and no supported installer available (npm or brew)"
  fi
  command -v codex >/dev/null 2>&1 || fail "Codex CLI install completed but codex is still not on PATH"
  codex --version >/dev/null 2>&1 || fail "Codex CLI is still unusable after install"
}

ensure_codex_login() {
  if codex login status >/dev/null 2>&1; then
    return
  fi
  log "Codex login status unavailable or not logged in; starting login flow"
  codex login || fail "Codex login failed"
}

probe_translator() {
  if ! command -v greentic-i18n-translator >/dev/null 2>&1; then
    command -v cargo-binstall >/dev/null 2>&1 || fail "greentic-i18n-translator not found and cargo-binstall is unavailable"
    log "installing greentic-i18n-translator via cargo-binstall"
    cargo binstall -y greentic-i18n-translator || fail "failed to install greentic-i18n-translator via cargo-binstall"
  fi
  command -v greentic-i18n-translator >/dev/null 2>&1 || fail "greentic-i18n-translator is still not on PATH after cargo-binstall"
  local help_output
  help_output="$(greentic-i18n-translator --help 2>&1 || true)"
  [[ -n "$help_output" ]] || fail "unable to inspect greentic-i18n-translator --help"
  if ! greentic-i18n-translator translate --help >/dev/null 2>&1; then
    fail "translator subcommand 'translate' is required but unavailable"
  fi
}

setup_codex_wrapper() {
  command -v codex >/dev/null 2>&1 || return 0
  local real_codex
  real_codex="$(command -v codex)"
  local wrapper_dir
  wrapper_dir="$(mktemp -d)"
  cat > "$wrapper_dir/codex" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == "exec" ]]; then
  shift
  exec "$real_codex" exec --skip-git-repo-check "\$@"
fi
exec "$real_codex" "\$@"
EOF
  chmod +x "$wrapper_dir/codex"
  export PATH="$wrapper_dir:$PATH"
}

run_translate() {
  while IFS= read -r locale; do
    [[ -n "$locale" ]] || continue
    log "translating locale: $locale"
    greentic-i18n-translator translate \
      --langs "$locale" \
      --en "$SOURCE_FILE" || fail "translate failed for locale $locale"
  done < <(python3 - "$LOCALES_FILE" <<'PY'
import json
import sys
with open(sys.argv[1], 'r', encoding='utf-8') as f:
    data = json.load(f)
for locale in data:
    if locale != "en":
        print(locale)
PY
)
}

run_validate_per_locale() {
  local failed=0
  while IFS= read -r locale; do
    [[ -n "$locale" ]] || continue
    if ! greentic-i18n-translator validate --langs "$locale" --en "$SOURCE_FILE"; then
      log "validate failed for locale: $locale"
      failed=1
    fi
  done < <(python3 - "$LOCALES_FILE" <<'PY'
import json
import sys
with open(sys.argv[1], 'r', encoding='utf-8') as f:
    data = json.load(f)
for locale in data:
    if locale != "en":
        print(locale)
PY
)
  return "$failed"
}

run_status_per_locale() {
  local failed=0
  while IFS= read -r locale; do
    [[ -n "$locale" ]] || continue
    if ! greentic-i18n-translator status --langs "$locale" --en "$SOURCE_FILE"; then
      log "status failed for locale: $locale"
      failed=1
    fi
  done < <(python3 - "$LOCALES_FILE" <<'PY'
import json
import sys
with open(sys.argv[1], 'r', encoding='utf-8') as f:
    data = json.load(f)
for locale in data:
    if locale != "en":
        print(locale)
PY
)
  return "$failed"
}

run_optional_checks() {
  if greentic-i18n-translator validate --help >/dev/null 2>&1; then
    log "running translator validate"
    if ! run_validate_per_locale; then
      fail "translator validate failed"
    fi
  else
    log "warning: translator validate command not available; skipping"
  fi
  if greentic-i18n-translator status --help >/dev/null 2>&1; then
    log "running translator status"
    run_status_per_locale || fail "translator status failed"
  else
    log "warning: translator status command not available; skipping"
  fi
}

[[ -f "$LOCALES_FILE" ]] || fail "missing locales file: $LOCALES_FILE"
[[ -f "$SOURCE_FILE" ]] || fail "missing source locale file: $SOURCE_FILE"

ensure_codex
setup_codex_wrapper
ensure_codex_login
probe_translator
run_translate
run_optional_checks
log "translations updated. Run cargo build to embed translations into WASM"
