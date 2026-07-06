#!/usr/bin/env bash
# Verify the vendored extension-provider WIT matches the pinned upstream revs.
#
# The runner hosts the `greentic:extension-provider` world at TWO generations
# so packs built against either can run (wave-4 C1, extension-error v2):
#   - v0_2_0/  is pinned to the v1.2.20-research rev (REV_V0_2_0)
#   - v0_1_0/  is pinned to the original 0.1.0 rev   (REV_V0_1_0)
#
# Bump the matching REV_* explicitly when re-vendoring to a new upstream commit.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

UPSTREAM_REPO="greentic-biz/greentic-designer-extensions"

# v1.2.20-research (extension-provider@0.2.0, extension-base@0.2.0).
REV_V0_2_0="dda7974e55daf09a743bb5f2f93104f2b6f7789e"
# Original 0.1.0 generation (extension-provider@0.1.0, extension-base@0.1.0).
REV_V0_1_0="8c619b72bd4c19cc7840b36a8ca9e0c05541430a"

WIT_DIR="crates/greentic-runner-host/wit-vendor/extension-provider"

# Map each upstream wit/<file> to its vendored location within a generation
# dir. The primary package (extension-provider) sits at the gen-dir root; its
# imported packages (extension-base, extension-host) live under deps/<pkg>/ so
# wasmtime bindgen can resolve the multi-package graph.
declare -A LOCAL_REL=(
    [extension-provider.wit]="extension-provider.wit"
    [extension-base.wit]="deps/extension-base/extension-base.wit"
    [extension-host.wit]="deps/extension-host/extension-host.wit"
)
FILES=(extension-base.wit extension-host.wit extension-provider.wit)
FAILED=0

verify_generation() {
    local subdir="$1"
    local rev="$2"
    local base="https://raw.githubusercontent.com/${UPSTREAM_REPO}/${rev}/wit"
    for f in "${FILES[@]}"; do
        local local_path="${WIT_DIR}/${subdir}/${LOCAL_REL[$f]}"
        if ! [ -f "$local_path" ]; then
            echo "FAIL: ${local_path} missing"
            FAILED=1
            continue
        fi
        local local_sha remote_sha
        local_sha=$(sha256sum "$local_path" | cut -d' ' -f1)
        remote_sha=$(curl -fsSL "${base}/${f}" | sha256sum | cut -d' ' -f1)
        if [ "$local_sha" != "$remote_sha" ]; then
            echo "FAIL: ${local_path} drift vs upstream@${rev:0:7}"
            echo "  local:  $local_sha"
            echo "  remote: $remote_sha"
            FAILED=1
        else
            echo "OK: ${local_path} matches upstream@${rev:0:7}"
        fi
    done
}

verify_generation "v0_2_0" "$REV_V0_2_0"
verify_generation "v0_1_0" "$REV_V0_1_0"

if [ "$FAILED" -ne 0 ]; then
    echo
    echo "To fix: either"
    echo "  (a) refresh local from upstream:"
    echo "      curl -fsSL https://raw.githubusercontent.com/${UPSTREAM_REPO}/<rev>/wit/<file> \\"
    echo "        > ${WIT_DIR}/<gen>/<file>"
    echo "  (b) bump REV_V0_2_0 / REV_V0_1_0 in scripts/verify-wit-sync.sh to match the vendored WIT."
    exit 1
fi
