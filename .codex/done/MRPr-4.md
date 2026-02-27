PR-04 — greentic-runner: Secrets/config hardening (production-safe scoping)

Repo: greenticai/greentic-runner
Goal: No cross-tenant leakage via env; always scoped lookups.

Changes

Ensure every secrets read goes through greentic-secrets with a SecretScope derived from TenantCtx (+ team/user if present).

Make env-secrets provider explicitly dev/test only (feature flag or config gate).

Ensure greentic-config is only used for runner process config, not tenant runtime secrets/config.

Tests

“tenant A cannot read tenant B secret” test using fake/memory secrets backend

“prod mode rejects env secrets provider” test
