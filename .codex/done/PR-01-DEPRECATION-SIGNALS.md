# PR-01-DEPRECATION-SIGNALS (greentic-runner): Add deprecation signals (docs-first; optional annotations)

**Date:** 2026-02-19  
**Repo:** `greentic-runner`  
**Type:** Non-behavioral (docs + annotations only)

## Why
We are not migrating the ecosystem today. We are making legacy **obvious** so humans and Codex stop building new work on top of it.

This PR adds **signals**, not migrations.

## Goals
1) Add clear Legacy/Deprecated/Compat labeling where developers look first:
   - docs sections
   - module-level docs
   - WIT world/interface comments (if applicable)
   - (optionally) Rust `#[deprecated]` on obvious legacy exports
2) Every legacy surface points to the canonical v0.6 replacement.

## Scope
### A) Docs-first deprecation markers (required)
- In `docs/vision/legacy.md`, add explicit “Do not use for new code” language at the top.
- Add **LEGACY banners** to any remaining legacy docs pages that cannot be removed yet.
- Add **CANONICAL banners** on v0.6 docs and examples.

### B) WIT legacy markers (required if repo contains WIT)
- Add a “LEGACY / COMPAT” banner comment at the top of legacy worlds/interfaces.
- If supported by your toolchain, add `@deprecated(...)` with version + message.
- Ensure canonical v0.6 worlds are explicitly marked CANONICAL.

### C) Rust `#[deprecated]` annotations (optional, safe-only)
Only do this where it’s low-risk and clearly legacy:
- public re-exports of old envelopes/manifests/context types
- “compat” helper functions that represent old contracts

Rules:
- Do not rename or move items in a way that breaks imports.
- Prefer deprecated aliases over relocation.
- Every `#[deprecated]` note must include:
  - canonical replacement path
  - link to `docs/vision/legacy.md#<anchor>`

## Repo-specific legacy surfaces to flag
Codex must enumerate in the PR description (or a short `audit` section):
- List top 10 legacy exports / docs sections / WIT worlds that should be labeled legacy.
- For each: replacement + link anchor.

## Acceptance criteria
- No behavior changes.
- Every flagged legacy surface has a clear replacement pointer.
- Canonical v0.6 path is visually dominant in docs.
- Builds/tests remain green.

## Repo-specific notes (greentic-runner)
Flag as legacy:
- Any invocation paths that rely on JSON-first or non-self-describing contracts
- Any docs that embed domain/provider specifics in runner
Canonical must emphasize:
- CBOR-first envelopes
- Policy boundaries and isolation
- Generic execution of self-describing components/packs
