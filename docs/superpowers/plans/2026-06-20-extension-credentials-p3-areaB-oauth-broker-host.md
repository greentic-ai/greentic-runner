# Extension Credentials P3 Area B — OAuth broker runner host Implementation Plan

> REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`).

> **Scope:** wire the runner-host as the **thin proxy** for `greentic:oauth-broker/broker-v1`: implement the
> `OAuthBrokerHost` trait so a consumer component's `get-token(provider, subject, scopes)` proxies (blocking
> HTTP) to the admin's `POST /resource-token` endpoint (shipped in Area A, greentic-admin#226). `get-consent-url`
> /`exchange-code` are operator-time (admin-only) → return empty string at runtime. Single repo: greentic-runner.
> Cut off `research`. Branch `feat/oauth-broker-host`. Target PR → `research`.

**Goal:** with `oauth:` configured, an extension that imports `broker-v1` and calls `get-token` receives a fresh
token JSON proxied from the admin (which resolves config + secret + lazy-refresh). No async runtime, no WASM
broker component — the host satisfies the import via a sync `func_wrap` calling a blocking HTTP POST.

**Tech:** Rust, wasmtime component linker (SYNC `func_wrap`), `reqwest::blocking`, serde, anyhow. Rust 1.95.
No `unwrap()`/`panic!()` in production paths; `anyhow`/`thiserror`.

## Global Constraints
- English only. Conventional commits, **no AI attribution / Co-Authored-By**. PR → `research`.
- NEVER log token/secret/bearer values. Reuse-first: the linker plumbing already exists (see below) — flip it on
  and implement the trait; do not re-architect.
- Gate: `cargo fmt --all -- --check` + `cargo clippy --all-targets --all-features -- -D warnings` +
  `cargo test -p greentic-runner-host`. (Full `bash ci/local_check.sh` if it runs in reasonable time; the wasm
  fixture tests may need `wasm32-wasip2` + `cargo-component` — if a fixture test is already `#[ignore]`/gated,
  keep that gating.) Run cargo raw; never pipe through `| tail` (masks exit code).

## Verbatim facts (from exploration — use exactly)
- **The host calls are SYNC.** `greentic-interfaces-wasmtime` `host_helpers/v1/oauth_broker.rs` exposes
  `pub use bindings::Host as OAuthBrokerHost` (trait) + `add_oauth_broker_to_linker<T>(linker, get: fn(&mut T) -> &mut dyn OAuthBrokerHost)`
  using `func_wrap` (sync). Trait methods:
  - `fn get_consent_url(&mut self, provider_id: String, subject: String, scopes: Vec<String>, redirect_path: String, extra_json: String) -> String`
  - `fn exchange_code(&mut self, provider_id: String, subject: String, code: String, redirect_path: String) -> String`
  - `fn get_token(&mut self, provider_id: String, subject: String, scopes: Vec<String>) -> String`
  (the `String` types are `wasmtime::component::__internal::String`/`Vec` — match the existing impls, e.g.
  `OfflineOAuthBroker` in greentic-flow `src/wizard_ops.rs` and `DummyOAuthBroker` in greentic-interfaces-wasmtime
  `tests/host_helpers_compile.rs` — READ one for the exact param types/signature to copy.)
- **Wiring point:** `crates/greentic-runner-host/src/pack.rs` ~line 1314, `add_all_v1_to_linker(linker, HostFns { … oauth_broker: None, … })`.
  Every other host import uses `Some(|state: &mut ComponentState| state.host_mut())`. Flip `oauth_broker` to the same.
  `state.host_mut()` returns `&mut HostState`, so **`HostState` must implement the `OAuthBrokerHost` bindings trait**
  (the `get` closure coerces `&mut HostState` to `&mut dyn OAuthBrokerHost`). READ the existing `impl HttpClientHostV1_1 for HostState`
  (same file) for how a host trait is implemented on `HostState` + the exact trait import path used in pack.rs.
- **HostState fields** (`pack.rs` ~247): `config: Arc<HostConfig>` (`config.tenant` is the tenant), `http_client: Arc<BlockingClient>`
  (`BlockingClient = reqwest::blocking::Client`, pack.rs:66), `default_env: String`, `oauth_config: Option<OAuthBrokerConfig>`,
  `oauth_host: OAuthBrokerHost` (the legacy empty struct — see cleanup below). `host_mut()` returns `&mut HostState`.
- **oauth types** (`crates/greentic-runner-host/src/oauth.rs`): `OAuthBrokerConfig { http_base_url, nats_url, default_provider: Option<String>, team: Option<String> }`;
  `ResourceTokenRequest { http_base_url, env, tenant, team: Option<String>, resource_id, scopes: Vec<String> }`;
  `ResourceTokenResponse { access_token: String, expires_at: u64 }`; async `request_resource_token(client: &reqwest::Client, &req)`
  (validates https/no-creds/no-query, joins `resource-token`, POST json, `error_for_status`, parse json); a dead no-op
  `add_oauth_broker_to_linker<T>(_linker) -> Result<()> { Ok(()) }` (oauth.rs:114 — DELETE it, it is dangling).
- **Config** (`crates/greentic-runner-host/src/config.rs`): `OAuthConfig { http_base_url, nats_url, provider, team }` (~171);
  `HostConfig::oauth_broker_config(&self) -> Option<OAuthBrokerConfig>` (~272) maps it.
- **CRITICAL integration gap:** the admin `/resource-token` (Area A) sits behind a shared-secret bearer middleware
  (`Authorization: Bearer <GREENTIC_ADMIN_RUNNER_SHARED_SECRET>`). The current `request_resource_token` sends NO auth →
  it would get 401. This branch MUST add the shared secret to the request as a bearer.
- Engine is `Engine::default()` (sync); HTTP host import already uses the blocking client (`send_http_request` → `builder.send()`).
  So: **no async, no `block_on` — use `reqwest::blocking` directly.**

---

### Task 1: blocking resource-token request + shared-secret config
**Files:** `crates/greentic-runner-host/src/oauth.rs`, `crates/greentic-runner-host/src/config.rs`.
- [ ] **Step 1 — shared secret in config:** add `pub shared_secret: Option<String>` to `OAuthConfig` (serde `#[serde(default)]`)
  and to `OAuthBrokerConfig`; map it in `oauth_broker_config()`. ALSO support an env override read at config build: prefer
  env `GREENTIC_OAUTH_BROKER_SHARED_SECRET` when set, else the yaml field (READ config.rs for any existing env-read pattern and
  match it; if none, read via `std::env::var(...).ok()` at the `oauth_broker_config()` mapping site). Never log the value.
- [ ] **Step 2 — blocking request:** add
  `pub fn request_resource_token_blocking(client: &reqwest::blocking::Client, request: &ResourceTokenRequest, shared_secret: Option<&str>) -> anyhow::Result<ResourceTokenResponse>`
  mirroring the async `request_resource_token` EXACTLY (same `validate_https_url_no_credentials_and_no_query` on
  `request.http_base_url`, same `base.join("resource-token")`), but blocking: `let mut rb = client.post(url).json(request); if let Some(s) = shared_secret { rb = rb.bearer_auth(s); } rb.send()?.error_for_status()?.json()`.
- [ ] **Step 3 — delete dead stub:** remove the no-op `add_oauth_broker_to_linker<T>(_linker)` in oauth.rs (dangling; real wiring is Task 2).
- [ ] **Step 4 — unit tests:** against a local stub (use whatever the crate already uses for an HTTP test double — search tests for
  `wiremock`/`TcpListener`/`httpmock`; if none, a one-shot `std::net::TcpListener` on `127.0.0.1:0` answering one POST with
  `{"access_token":"AT","expires_at":1700000000}`): assert the blocking request returns the parsed response; assert the
  `Authorization: Bearer <secret>` header IS present when `shared_secret` is Some and ABSENT when None; assert an `http://`
  base url is rejected by the https validation. Keep existing `oauth_broker_config_*` tests green (extend for `shared_secret`).
- [ ] **Step 5:** fmt + clippy + `cargo test -p greentic-runner-host oauth`. Commit: `feat(runner-host): blocking resource-token request + shared-secret bearer`.

### Task 2: implement OAuthBrokerHost for HostState + wire the linker
**Files:** `crates/greentic-runner-host/src/pack.rs` (impl + flip the HostFns flag); possibly `src/oauth.rs` (cleanup).
- [ ] **Step 1 — impl the bindings trait on `HostState`:** add `impl <oauth_broker bindings path>::OAuthBrokerHost for HostState`
  (use the SAME import path pack.rs already uses to reference the interfaces host traits — find how `HttpClientHostV1_1` /
  the `add_all_v1_to_linker`/`HostFns` symbols are imported and reuse that module path for `OAuthBrokerHost`). Methods:
  - `get_token(provider_id, subject, scopes)`: `let Some(cfg) = self.oauth_config.clone() else { return String::new() };`
    build `ResourceTokenRequest { http_base_url: cfg.http_base_url, env: self.default_env.clone(), tenant: self.config.tenant.clone(), team: cfg.team.clone(), resource_id: provider_id.to_string(), scopes: scopes.into_iter().map(Into::into).collect() }`;
    call `request_resource_token_blocking(&self.http_client, &req, cfg.shared_secret.as_deref())`; on `Ok(resp)` →
    `serde_json::to_string(&resp).unwrap_or_default()` (returns the `{access_token, expires_at}` token JSON the consumer reads);
    on `Err(e)` → `tracing::warn!(provider = %provider_id, error = %e, "oauth get-token proxy failed")` (NO secret/token in the log) and return `String::new()`.
    `subject` is informational for MVP (host tenant/env/team are authoritative); document that in a comment — do NOT derive tenant from `subject`.
  - `get_consent_url(..) -> String` and `exchange_code(..) -> String`: return `String::new()` (operator-time, unsupported at
    runtime — Connect happens in the admin). Add a one-line doc comment saying so.
- [ ] **Step 2 — wire the linker:** in `pack.rs` `register_all`, change `oauth_broker: None` to
  `oauth_broker: Some(|state: &mut ComponentState| state.host_mut())`.
- [ ] **Step 3 — dead-code cleanup:** grep for `oauth_host`, `OAuthHostContext`, and the legacy empty `OAuthBrokerHost` struct in
  oauth.rs. If implementing the bindings trait on `HostState` makes the local empty `OAuthBrokerHost` struct + `oauth_host` field +
  `OAuthHostContext` trait/impl unused, remove them (grep-confirm zero remaining uses first). If anything still references them, leave
  them and note it in the report. Do NOT leave `#[allow(dead_code)]` to paper over it.
- [ ] **Step 4:** fmt + clippy + `cargo test -p greentic-runner-host`. Commit: `feat(runner-host): wire OAuthBrokerHost get-token proxy into the component linker`.

### Task 3: end-to-end test (get-token proxies; consent/exchange empty)
**Files:** extend `crates/greentic-runner-host/tests/oauth_broker.rs` (or add a focused test module).
- [ ] **Step 1 — direct host-impl test:** construct a `HostState` whose `oauth_config` points `http_base_url` at a local stub
  `/resource-token` returning `{"access_token":"AT","expires_at":1700000000}` and whose `shared_secret = Some("s3cr3t")`; call
  `OAuthBrokerHost::get_token(&mut host, "demo".into(), "subject".into(), vec!["scope".into()])`; assert the returned String
  parses to `{access_token:"AT", expires_at:1700000000}` AND the stub received `Authorization: Bearer s3cr3t`. Assert
  `get_consent_url(..)`/`exchange_code(..)` return `""`. Assert that with `oauth_config = None`, `get_token` returns `""`
  (no panic, no request). (READ the existing test for how `HostState`/`HostConfig` are constructed in-test and reuse that helper.)
- [ ] **Step 2 — keep instantiation green:** the existing `tests/oauth_broker.rs` instantiation test must still pass now that
  `register_all` wires `oauth_broker`. If a fixture consumer would actually call `get-token` during the test, point its config at the
  stub (or assert instantiation succeeds without calling out). Do not weaken existing assertions.
- [ ] **Step 3:** fmt + clippy + `cargo test -p greentic-runner-host oauth`. Commit: `test(runner-host): get-token proxy round-trip + consent/exchange unsupported`.

### Task 4: full gate + docs
- [ ] **Step 1:** `cargo fmt --all -- --check` + `cargo clippy --all-targets --all-features -- -D warnings` +
  `cargo test -p greentic-runner-host`. (Attempt `bash ci/local_check.sh`; if it requires the wasip2 toolchain/cargo-component for
  fixtures and that's not set up, note exactly what was skipped in the report — do not fake it.) Fix anything in changed files.
- [ ] **Step 2:** add an "Area B — IMPLEMENTED" note to `docs/superpowers/plans/2026-06-20-extension-credentials-p3-areaB-oauth-broker-host.md`
  (deviations + the deploy requirement: runner must be configured with `oauth.http_base_url` = the admin's `/api/runner/` base AND
  `oauth.shared_secret`/`GREENTIC_OAUTH_BROKER_SHARED_SECRET` = the admin's `GREENTIC_ADMIN_RUNNER_SHARED_SECRET`). Commit:
  `docs(runner-host): mark P3 Area B implemented + deploy config note`.

## Area B — IMPLEMENTED (2026-06-20, branch `feat/oauth-broker-host` → research)

Built subagent-driven (implement → review → fix per task). Commits: `568195be` (blocking request + shared-secret),
`e71ec4cc` (OAuthBrokerHost impl + linker flip + dead-code removal), `e1ca1462` (tests).

Shipped:
- `oauth.rs`: `request_resource_token_blocking(client, request, shared_secret)` — mirrors the async sibling's
  https-only/no-creds/no-query validation + `resource-token` URL join, adds `.bearer_auth(secret)` when present.
  Deleted the dangling no-op `add_oauth_broker_to_linker` stub.
- `config.rs`: `OAuthConfig.shared_secret` + `OAuthBrokerConfig.shared_secret`, mapped in `oauth_broker_config()`
  with an env override (`GREENTIC_OAUTH_BROKER_SHARED_SECRET` wins over the yaml field).
- `pack.rs`: `impl OAuthBrokerHost for HostState` — `get_token(provider, _subject, scopes)` builds the
  `ResourceTokenRequest` from the **host context** (`config.tenant` / `default_env` / config `team`, NOT from the
  wasm-supplied `subject`), proxies via the blocking request, returns the `{access_token, expires_at}` JSON on
  success or `""` on error (logs provider+error only, never the secret/token). `get_consent_url`/`exchange_code`
  return `""` (operator-time, admin-only). Flipped `HostFns.oauth_broker: None → Some(|state| state.host_mut())`.
  Removed the now-dead legacy `OAuthBrokerHost` struct + `oauth_host` field + `OAuthHostContext` trait/impl.

Gate: `cargo fmt --all -- --check` ✅; `cargo clippy -p greentic-runner-host --features verify --all-targets -D warnings` ✅;
`cargo test -p greentic-runner-host --features verify` ✅. **Known limitations (NOT this branch's defects):** the
default `agentic-worker` feature fails to compile due to a PRE-EXISTING cross-repo drift in the sibling path-dep
`greentic-aw-runtime` (`config::AgentConfig` missing field `guardrails`, serve.rs) — this branch touches none of
those files and the error reproduces on the research baseline; and `--all-features` can't build locally
(`surrealdb-librocksdb-sys` needs system clang headers for the `knowledge-chronicle` feature). The
get-token SUCCESS+bearer-on-the-wire path is not unit-tested here: the request is https-only and the crate's only
HTTP test double (wiremock) is http-only — it is read-review-confirmed and exercised admin-side by Area A's
wiremock test of `/resource-token`.

**Deploy requirement (devops):** configure the runner's `oauth:` block with `http_base_url` = the admin's
`/api/runner/` base URL (note the trailing slash — the request joins `resource-token` onto it) and
`shared_secret` (or env `GREENTIC_OAUTH_BROKER_SHARED_SECRET`) = the admin's `GREENTIC_ADMIN_RUNNER_SHARED_SECRET`.

## Self-Review
- get-token proxy (blocking, bearer-authed, tenant/env/team from host ctx) → Tasks 1-2; consent/exchange empty → Task 2;
  end-to-end proof → Task 3. Reuse-first: flips the existing `HostFns.oauth_broker` flag, mirrors `request_resource_token` +
  the existing host-trait impl pattern; deletes dead code rather than forking.
- Wire-contract: `ResourceTokenRequest`/`ResourceTokenResponse` already match the admin's Area A structs; the only addition is
  the bearer header (the integration gap).

## Out of scope
- Async linker / `func_wrap_async` (host is sync — explicitly not needed). NATS path. `get-consent-url`/`exchange-code` runtime impl
  (admin-only). The admin SPA Connect button. Deploying the runner with the env vars (devops; documented in Task 4 note).
