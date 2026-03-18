# PR-00-VISION (greentic-runner): Add v0.6 vision docs + Codex rules + deprecation policy

**Date:** 2026-02-19  
**Repo:** `greentic-runner`  
**Type:** Docs/policy only (no behavior change)

## Why
Greentic is moving to **component v0.6.0** and **packs with extensions/validators**. This repo must clearly communicate:
- what is canonical v0.6 usage,
- what is legacy/compat,
- how Codex contributors should implement changes going forward.

Mixed docs are a primary driver of inconsistencies and bugs.

## Repo purpose (v0.6)
Runtime executor for components/packs; CBOR envelopes; policy boundaries; no domain specifics.

## Goals
1) Make the **canonical v0.6 way** unmistakable in this repo.  
2) Make **legacy** unmistakable (explicitly “not for new code”).  
3) Provide **Codex instructions** so future work stays aligned.  
4) Establish a **deprecation policy** (Rust + WIT) used in follow-up PRs.

## Scope (files to add)
Create folder `docs/vision/` with:

- `docs/vision/v0.6.md` — repo-specific vision & correct usage
- `docs/vision/legacy.md` — legacy inventory + what replaces it
- `docs/vision/codex.md` — Codex instructions for future PRs
- `docs/vision/deprecations.md` — how to mark legacy (Rust + WIT) + message templates
- (optional) `docs/vision/_snippets.md` — canonical/legacy banners for reuse

Update primary docs:
- README (or docs index) links to `docs/vision/v0.6.md` and `docs/vision/legacy.md`.

## Content requirements

### `docs/vision/v0.6.md` MUST answer
- What this repo is for in v0.6 (1 paragraph)
- What this repo is NOT for (explicit exclusions)
- Canonical entrypoints/modules/WIT worlds (bullets)
- Do this (3–6 “correct usage” bullets)
- Don’t do this (3–6 “anti-pattern” bullets)
- How this repo supports: multi-tenant, CBOR-first, self-description, extensions/validators, security boundaries
- Links to canonical docs in related repos

### `docs/vision/codex.md` MUST include
- Default posture: prefer canonical v0.6 APIs and docs
- Never introduce new legacy exports/worlds
- If you touch public API or docs, update `docs/vision/*`
- If legacy remains for compat, label it + provide migration pointer
- No provider/domain knowledge in core repos

### `docs/vision/legacy.md` MUST include
- A table: legacy surface → replacement → why legacy exists → removal milestone (TBD allowed)
- Clear statement: legacy is allowed only for compatibility

### `docs/vision/deprecations.md` MUST include
- Rust `#[deprecated]` examples and note template
- WIT legacy banner + `@deprecated` guidance (if supported)
- Rule: every deprecation must point to canonical replacement and an anchor in `legacy.md`

## Codex execution checklist
- [ ] Create the `docs/vision/` files using the templates above
- [ ] Add links from README/docs index
- [ ] Ensure docs do not describe legacy as “recommended”
- [ ] No code behavior changes
- [ ] `cargo test` / existing checks remain green

## Acceptance criteria
- A new contributor can find the canonical approach in under 60 seconds.
- Legacy guidance is segregated and clearly labeled.
- No functional changes.

## Repo-specific notes (greentic-runner)
Flag as legacy:
- Any invocation paths that rely on JSON-first or non-self-describing contracts
- Any docs that embed domain/provider specifics in runner
Canonical must emphasize:
- CBOR-first envelopes
- Policy boundaries and isolation
- Generic execution of self-describing components/packs
