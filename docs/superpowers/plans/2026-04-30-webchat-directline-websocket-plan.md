# WebChat Direct Line WebSocket — Implementation Plan

> **Reference spec:** `docs/superpowers/specs/2026-04-30-webchat-directline-websocket-design.md`
>
> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Production-ready Direct Line v3 WebSocket transport in `greentic-start` (the existing hyper-based HTTP ingress for `/v1/messaging/...`), with managed-Redis pub/sub fan-out for multi-replica horizontal scale, full graceful shutdown integration, resource limits, observability, and AWS/GCP/Azure deployment configs.

**Architecture:** Hyper-based WS upgrade in `greentic-start/src/http_ingress/webchat_ws/`, using `hyper::upgrade::on(req)` plus `tokio-tungstenite::WebSocketStream::from_raw_socket` (or the `hyper-tungstenite` helper). JWT-authenticated at upgrade via existing `verify_token` (linked from `webchat-directline-core` crate). Per-replica connection registry with bounded `mpsc` channels. Redis Pub/Sub backplane for cross-replica push (`webchat:activity:{tenant}:{conv_id}` channels). Server-emitted activities intercepted from `HttpOutV1.events` after WASM `ingest_http` returns and published to Redis. SIGTERM-aware graceful drain via the existing oneshot signal.

**Tech stack:** Rust 1.95 (greentic-start MSRV), hyper 1 (already used), `hyper-tungstenite` 0.17 + `tokio-tungstenite` 0.24 for the WS layer, `redis` 0.27+ async (`tokio-comp`, `connection-manager`, `tls-native-tls` features), `serde_json`, `tracing`, `metrics` (via shared `greentic-telemetry`).

**Total estimated effort:** 13-18 days across 8 phases. Phases C/D/E parallelizable across subagents.

---

## File structure

> **Implementation host:** `greentic-start` (standalone crate, raw hyper). The `/v1/messaging/...` ingress already lives there; `greentic-runner-host` only serves admin/operator routes and is NOT the right host for customer ingress changes. All paths below are relative to the `greentic-start` repo root unless otherwise noted. Cloud deploy configs and the runbook live in `greentic-runner` since they describe the operator container produced by that repo.

### New files (in `greentic-start`)

| Path | Purpose |
|------|---------|
| `src/http_ingress/webchat_ws/mod.rs` | hyper-based WS upgrade handler, registry, frame loop |
| `src/http_ingress/webchat_ws/registry.rs` | per-replica `HashMap<ConvId, Vec<WsSender>>` + refcounted Redis subs |
| `src/http_ingress/webchat_ws/redis_backplane.rs` | publisher + subscriber over `redis::aio::ConnectionManager` |
| `src/http_ingress/webchat_ws/auth.rs` | JWT extraction + `verify_token` integration |
| `src/http_ingress/webchat_ws/limits.rs` | per-tenant / per-IP / per-replica connection caps |
| `src/http_ingress/webchat_ws/metrics.rs` | Prometheus metric definitions + helpers |
| `tests/webchat_ws_e2e.rs` | end-to-end integration test (single replica) |
| `tests/webchat_ws_multi_replica.rs` | two-replica + Redis testcontainers test |
| `tests/webchat_ws_chaos.rs` | network partition / Redis kill / slow consumer scenarios |
| `benches/webchat_ws_load.rs` | criterion bench: 10k concurrent connections |

### New files (in `greentic-runner`)

| Path | Purpose |
|------|---------|
| `deploy/aws/webchat-ws.tf` | Terraform: ALB tweaks, target group, ECS task `stopTimeout`, ElastiCache Serverless |
| `deploy/gcp/webchat-ws.sh` (+ `webchat-ws.service.yaml`) | `gcloud run deploy` with WS-tuned flags + Memorystore + sample Knative manifest |
| `deploy/azure/webchat-ws.bicep` (+ `webchat-ws.parameters.json`) | Bicep: premium ingress, container app scale rules, Azure Cache for Redis |
| `docs/runbooks/webchat-ws.md` | on-call runbook: connection-pool saturation, Redis outage response, replica drain mid-deploy |

### Modified files (in `greentic-start`)

| Path | Change |
|------|--------|
| `Cargo.toml` | add `redis = { version = "0.27", features = ["tokio-comp", "connection-manager", "tls-native-tls", "tokio-native-tls-comp"] }`, `tokio-tungstenite = "0.24"`, `hyper-tungstenite = "0.17"`, `tokio-stream = "0.1"`. New feature flag `webchat-ws` (default on). |
| `src/http_ingress/mod.rs` | inspect `Upgrade: websocket` header and route the request into the new module before normal HTTP dispatch |
| `src/http_routes.rs` | add explicit `/v1/messaging/webchat/{tenant}/v3/directline/conversations/{conv_id}/stream` route entry (already covered by the wildcard but should be elevated for routing clarity + WS-specific behavior) |
| `src/ingress_dispatch.rs` | hook envelope-emission interceptor to publish on Redis after WASM `ingest_http` returns events for `provider=messaging-webchat-gui` |
| `src/messaging_app.rs` | inject Redis backplane handle into `HttpIngressState` |
| `src/lib.rs` | re-export `webchat_ws` types if needed by callers |

### Modified files (in `greentic-messaging-providers`)

| Path | Change |
|------|--------|
| `components/messaging-provider-webchat/src/directline/http.rs` | implement `POST /v3/directline/tokens/refresh` (currently 404) |

---

## Phase A — Alignment (Days 1-2, this PR)

### Task 1: Spec + plan docs review

**Files:**
- `docs/superpowers/specs/2026-04-30-webchat-directline-websocket-design.md` (this work)
- `docs/superpowers/plans/2026-04-30-webchat-directline-websocket-plan.md` (this file)

- [x] Draft spec
- [x] Draft plan
- [ ] PR open + assigned for stakeholder review
- [ ] Address review comments (likely sections: §17 open questions, §13 rollout strategy)
- [ ] Spec marked **Approved** in §1 status header

---

## Phase B — Core WS handler (Days 3-7)

### Task 2: Add redis dependency

**Files:**
- Modify: workspace `Cargo.toml`
- Modify: `Cargo.toml`

- [ ] **Step 1:** Add to `[workspace.dependencies]`:
  ```toml
  redis = { version = "0.27", features = ["tokio-comp", "connection-manager", "tls-native-tls"], default-features = false }
  ```
- [ ] **Step 2:** Add to the `[dependencies]` table: `redis.workspace = true`
- [ ] **Step 3:** Add feature `webchat-ws = []` (default-on) to greentic-start
- [ ] **Verify:** `cargo build -p greentic-start` succeeds
- [ ] **Verify:** `cargo build -p greentic-start --no-default-features` (without `webchat-ws`) succeeds — proves feature gate works

### Task 3: Module skeleton

**Files:**
- Create: `src/http_ingress/webchat_ws/mod.rs`
- Create: `src/http_ingress/webchat_ws/auth.rs`
- Create: `src/http_ingress/webchat_ws/registry.rs`
- Create: `src/http_ingress/webchat_ws/redis_backplane.rs`
- Create: `src/http_ingress/webchat_ws/limits.rs`
- Create: `src/http_ingress/webchat_ws/metrics.rs`
- Modify: `src/http_ingress/mod.rs`

- [ ] **Step 1:** Create empty modules with `pub` re-exports stub
- [ ] **Step 2:** Wire into `http/mod.rs` behind `#[cfg(feature = "webchat-ws")]`
- [ ] **Verify:** `cargo build -p greentic-start`

### Task 4: WS upgrade handler with JWT auth

**Files:**
- Modify: `src/http_ingress/webchat_ws/mod.rs`
- Modify: `src/http_ingress/webchat_ws/auth.rs`
- Modify: `src/http_routes.rs`

- [ ] **Step 1:** Implement `auth.rs`:
  - `extract_token_from_query(uri: &Uri) -> Option<String>` — parse `?t=`
  - `verify_directline_token(token: &str, secrets: &impl SecretStore) -> Result<TokenClaims, AuthError>` — link to existing `webchat_directline_core::directline::jwt::verify_token`
  - `validate_path_match(claims: &TokenClaims, conv_id: &str, env: &str, tenant: &str) -> Result<(), AuthError>`
- [ ] **Step 2:** Implement axum handler `handler_ws_upgrade(ws: WebSocketUpgrade, Path((conv_id,)): Path<(String,)>, Query(params): Query<UpgradeQuery>, State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse`:
  - Verify origin against allowlist (`limits::check_origin`)
  - Verify token, return 401 on failure (DO NOT upgrade)
  - Validate path/conv_id/tenant match
  - Check connection limits (`limits::check_admit`); return 429 / 503 as appropriate
  - Call `ws.on_upgrade(move |socket| handle_socket(socket, conv_id, claims, state))`
- [ ] **Step 3:** Implement `handle_socket(socket, conv_id, claims, state)`:
  - Register in registry
  - Run watermark catch-up replay (Task 6)
  - Spawn read loop (drain client frames, log warnings)
  - Spawn write loop (drain `mpsc::Receiver<Frame>` → ws.send)
  - Spawn ping task (every 25 s)
  - Wait for any task to terminate; clean up
- [ ] **Step 4:** Bind route `GET /v3/directline/conversations/:id/stream` in router
- [ ] **Verify:** Unit tests in `auth.rs` for token extraction, path validation, expired token rejection
- [ ] **Verify:** Manual smoke test with `wscat`:
  ```bash
  wscat -c 'ws://localhost:8080/v1/messaging/webchat/demo/v3/directline/conversations/abc/stream?t=<valid_jwt>&watermark=0'
  ```

### Task 5: Connection registry

**Files:**
- Modify: `src/http_ingress/webchat_ws/registry.rs`

- [ ] **Step 1:** Define `ConnKey { tenant: String, conv_id: String }`
- [ ] **Step 2:** Define `ConnRegistry`:
  - `connections: DashMap<ConnKey, Vec<WsSender>>`
  - `redis_subs: DashMap<String, Arc<AtomicU32>>` (channel → refcount)
- [ ] **Step 3:** Methods:
  - `register(key, sender) -> bool` — returns true if first conn for this key (caller subscribes Redis)
  - `unregister(key, sender_id) -> bool` — returns true if last conn (caller unsubscribes Redis)
  - `push(key, frame)` — broadcasts to all senders for this key
  - `connection_count(tenant) -> usize`
  - `connection_count_total() -> usize`
- [ ] **Step 4:** Bounded `mpsc::channel(64)` per `WsSender`; if `try_send` fails, mark slow consumer + drop.
- [ ] **Verify:** Unit tests for register/unregister refcount, push fan-out, slow-consumer drop

### Task 6: Watermark catch-up replay

**Files:**
- Modify: `src/http_ingress/webchat_ws/mod.rs`

- [ ] **Step 1:** On socket open, read `watermark=N` from query
- [ ] **Step 2:** Construct synthetic `HttpInV1` matching the polling `GET /activities` shape
- [ ] **Step 3:** Invoke WASM `ingest_http` via existing host runtime (this is the same path the polling endpoint uses)
- [ ] **Step 4:** Parse returned `HttpOutV1.body_b64` → `ActivitySet`; if non-empty, send as one WS frame
- [ ] **Verify:** Integration test: post 3 activities, then connect WS with `watermark=0`; verify all 3 delivered as a single frame.
- [ ] **Verify:** Same with `watermark=2` → only the last activity delivered

### Task 7: Keepalive ping/pong

**Files:**
- Modify: `src/http_ingress/webchat_ws/mod.rs`

- [ ] **Step 1:** Spawn ping task: `tokio::time::interval(Duration::from_secs(25))`
- [ ] **Step 2:** Track `last_pong_at: Arc<AtomicU64>`
- [ ] **Step 3:** On pong frame received, update `last_pong_at`
- [ ] **Step 4:** If `now - last_pong_at > 35s`, close `1011`
- [ ] **Verify:** Integration test: connect, suppress pong replies, expect close after ~35 s

---

## Phase C — Redis backplane (Days 8-10)

### Task 8: Redis publisher

**Files:**
- Modify: `src/http_ingress/webchat_ws/redis_backplane.rs`

- [ ] **Step 1:** Define `Backplane { manager: redis::aio::ConnectionManager }` constructor from URL
- [ ] **Step 2:** Implement `publish(channel: &str, payload: &[u8])` with retry + structured-log on failure
- [ ] **Step 3:** Define channel name builder: `webchat_activity_channel(tenant, conv_id) -> String`
- [ ] **Verify:** Unit test against `redis-server` via testcontainers; publish then SUBSCRIBE in same test verifies delivery.

### Task 9: Redis subscriber loop

**Files:**
- Modify: `src/http_ingress/webchat_ws/redis_backplane.rs`

- [ ] **Step 1:** Maintain dedicated subscriber connection (Pub/Sub mode blocks other commands)
- [ ] **Step 2:** Spawn task that reads messages from subscriber and dispatches to `ConnRegistry::push(key_from_channel, payload)`
- [ ] **Step 3:** Handle reconnect: on disconnect, re-subscribe ALL active channels from `ConnRegistry::redis_subs.keys()`
- [ ] **Verify:** Chaos test: kill Redis container mid-flight, verify replicas resubscribe and resume push within 10 s

### Task 10: Hook publish into envelope emission

**Files:**
- Modify: `src/ingress_dispatch.rs` (or wherever `HttpOutV1` events are processed)

- [ ] **Step 1:** Locate the path where runner-host receives `HttpOutV1` from WASM `ingest_http` and routes envelopes
- [ ] **Step 2:** Add an interceptor: for each envelope where `metadata.get("provider") == Some("messaging-webchat-gui")`, derive channel from `(metadata.tenant, conversation_id)` and call `Backplane::publish`
- [ ] **Step 3:** On publish error, log warning + metric increment; DO NOT fail the HTTP response — local registry push still works for same-replica subscribers
- [ ] **Verify:** Multi-replica integration test: client A on replica 1, POST activity to replica 2; client A receives within 200 ms

### Task 11: Subscribe/unsubscribe lifecycle integration

**Files:**
- Modify: `src/http_ingress/webchat_ws/mod.rs`
- Modify: `src/http_ingress/webchat_ws/registry.rs`

- [ ] **Step 1:** On `ConnRegistry::register(key, _)` returning `true` (first conn for this key), call `Backplane::subscribe(channel)`
- [ ] **Step 2:** On `ConnRegistry::unregister(key, _)` returning `true` (last conn), call `Backplane::unsubscribe(channel)`
- [ ] **Verify:** Unit test for refcount: register twice, unregister once → still subscribed; unregister second → unsubscribed

---

## Phase D — Hardening (Days 11-12)

### Task 12: Resource limits

**Files:**
- Modify: `src/http_ingress/webchat_ws/limits.rs`

- [ ] **Step 1:** Per-tenant connection cap (default 10000, configurable)
- [ ] **Step 2:** Per-replica connection cap (default 5000)
- [ ] **Step 3:** Per-IP connection cap (default 50)
- [ ] **Step 4:** Per-IP upgrade rate limit (default 5/min) — token-bucket via existing rate-limit pattern
- [ ] **Step 5:** Frame size limit (default 256 KiB) — enforced on outgoing push
- [ ] **Step 6:** Slow-consumer threshold — drop after `mpsc` send fails (handled in registry)
- [ ] **Verify:** Integration tests for each limit tripping the right HTTP code (429, 503)

### Task 13: Origin allowlist

**Files:**
- Modify: `src/http_ingress/webchat_ws/limits.rs`
- Modify: tenant config schema

- [ ] **Step 1:** Read `webchat.ws_origin_allowlist` from tenant config
- [ ] **Step 2:** Match incoming `Origin` header against list (support `*` wildcard, exact, suffix-match for `*.example.com`)
- [ ] **Step 3:** Default in dev: `["*"]` with `WARN` log every time wildcard matches
- [ ] **Verify:** Tests for exact, suffix, wildcard, mismatch

### Task 14: Token refresh endpoint

**Files:**
- Modify: `components/messaging-provider-webchat/src/directline/http.rs` (in `greentic-messaging-providers`)

- [ ] **Step 1:** Implement `POST /v3/directline/tokens/refresh`:
  - Bearer token in `Authorization` header
  - Verify token (allow expired-recently? Microsoft spec: yes, within 24 h grace)
  - Issue new token bound to same `(env, tenant, conv_id, user_id)`
  - Return same shape as `POST /tokens/generate`
- [ ] **Step 2:** Add tests in `directline::http::tests`
- [ ] **Verify:** `cargo test -p webchat-directline-core` passes

### Task 15: Token-expiring close

**Files:**
- Modify: `src/http_ingress/webchat_ws/mod.rs`

- [ ] **Step 1:** On socket open, schedule `tokio::time::sleep_until(claims.exp - 180s)`
- [ ] **Step 2:** On wake, send `Close { code: 1008, reason: "token_expiring" }` and let read/write loops terminate naturally
- [ ] **Verify:** Integration test with short TTL — connect, wait, verify close code received by client

---

## Phase E — Observability (Days 13)

### Task 16: Prometheus metrics

**Files:**
- Modify: `src/http_ingress/webchat_ws/metrics.rs`

- [ ] **Step 1:** Define metrics per spec §10.1:
  - `webchat_ws_connections_active` (gauge, labels: tenant, env)
  - `webchat_ws_connections_total` (counter, labels: tenant, env)
  - `webchat_ws_disconnects_total` (counter, labels: tenant, reason)
  - `webchat_ws_push_latency_seconds` (histogram, labels: tenant)
  - `webchat_ws_dropped_frames_total` (counter, labels: tenant, reason)
  - `webchat_ws_redis_subscriptions` (gauge)
  - `webchat_ws_redis_publish_total` (counter, labels: result)
- [ ] **Step 2:** Wire metrics into registry/handler/backplane code paths
- [ ] **Verify:** `curl http://localhost:8080/metrics` shows all metrics with sane values after running integration tests

### Task 17: Tracing & structured logs

**Files:**
- Modify: `src/http_ingress/webchat_ws/mod.rs`

- [ ] **Step 1:** Create per-connection span: `info_span!("webchat_ws", conv_id, tenant, env, replica_id)`
- [ ] **Step 2:** Log open/close at INFO with full context
- [ ] **Step 3:** Log warnings (slow_consumer, pong_timeout, redis_publish_failed) at WARN with structured fields
- [ ] **Step 4:** Propagate trace context from incoming `HttpInV1` headers to outgoing publish events (existing greentic-telemetry pattern)
- [ ] **Verify:** Trace inspection in dev: open WS, post activity from another shell, check that the trace span links the publish → push events

---

## Phase F — Tests (Days 14-16)

### Task 18: Unit tests

**Files:**
- Modify: `src/http_ingress/webchat_ws/{auth,registry,limits,redis_backplane,metrics}.rs`

- [ ] All edge cases per spec §15.1
- [ ] **Verify:** `cargo test -p greentic-start webchat_ws::` covers all modules; `cargo llvm-cov` shows ≥95% line coverage on new files

### Task 19: Integration tests (single replica)

**Files:**
- Create: `tests/webchat_ws_e2e.rs`

- [ ] End-to-end: client connects → POST activity → client receives frame
- [ ] Token expiry triggers `1008 token_expiring`
- [ ] Origin allowlist rejects spoof
- [ ] Per-IP upgrade rate limit triggers 429
- [ ] Graceful shutdown drains all WS within budget
- [ ] **Verify:** All scenarios pass with `cargo test -p greentic-start --test webchat_ws_e2e`

### Task 20: Integration tests (multi-replica)

**Files:**
- Create: `tests/webchat_ws_multi_replica.rs`

- [ ] Spin two `hyper Service` instances + Redis testcontainers
- [ ] Client A on replica 1, bot reply produced via replica 2's HTTP path → verify delivery within 200 ms
- [ ] Watermark catch-up: simulate 1000 missed messages, verify all delivered in order
- [ ] **Verify:** `cargo test -p greentic-start --test webchat_ws_multi_replica`

### Task 21: Chaos tests

**Files:**
- Create: `tests/webchat_ws_chaos.rs`

- [ ] Kill Redis container mid-traffic → verify reconnect + RE-SUBSCRIBE
- [ ] Force replica restart with active connections → clients reconnect within 5 s
- [ ] Slow-network client → slow-consumer drop kicks in
- [ ] **Verify:** Tests pass; ignored by default in CI (run nightly via `RUN_CHAOS=1`)

### Task 22: Load benchmark

**Files:**
- Create: `benches/webchat_ws_load.rs`

- [ ] Criterion bench with 1k / 5k / 10k concurrent WS
- [ ] Measure: upgrade rate, push latency p50/p99, memory per connection
- [ ] **Verify:** Bench runs to completion; results documented in `docs/runbooks/webchat-ws.md` perf section

---

## Phase G — Cloud deployment configs (Days 16-17)

### Task 23: AWS

**Files:**
- Create: `deploy/aws/webchat-ws.tf`

- [ ] Terraform module per spec §12.1
- [ ] ALB tweaks (idle_timeout 600, app_cookie stickiness)
- [ ] Target group `deregistration_delay = 110`
- [ ] ECS task `stopTimeout: 110`
- [ ] ElastiCache Serverless instance + security group rules
- [ ] CloudWatch metric scaling Lambda + Application Auto Scaling target tracking
- [ ] **Verify:** `terraform plan` produces clean diff against existing webchat ECS module
- [ ] **Verify:** Documented integration with `greentic-deployer-extensions/reference-extensions/deploy-aws`

### Task 24: GCP

**Files:**
- Create: `deploy/gcp/webchat-ws.sh`

- [ ] `gcloud run deploy` script per spec §12.2
- [ ] Memorystore Redis Standard tier provisioning (separate `gcloud redis instances create`)
- [ ] Direct VPC egress configuration
- [ ] **Verify:** Smoke deploy in dev project; WS upgrade succeeds; multi-replica push verified

### Task 25: Azure

**Files:**
- Create: `deploy/azure/webchat-ws.bicep`

- [ ] Bicep module per spec §12.3
- [ ] Premium ingress workload profile (Ingress-D4 minimum)
- [ ] Container app with sticky sessions, scale rules, `terminationGracePeriodSeconds: 110`
- [ ] Azure Cache for Redis Standard
- [ ] **Verify:** `az deployment group what-if` shows expected diff
- [ ] **Verify:** Smoke deploy; WS upgrade + multi-replica verified

---

## Phase H — Operations & rollout (Day 18)

### Task 26: Runbook

**Files:**
- Create: `docs/runbooks/webchat-ws.md`

- [ ] Section: connection-pool saturation — symptoms, dashboard, scale-out
- [ ] Section: Redis outage — symptoms, fallback (local-only), recovery
- [ ] Section: replica drain mid-deploy — graceful drain procedure, alternatives if drain hangs
- [ ] Section: stuck connections / pong timeout storms — diagnosis, mitigation
- [ ] Section: token-expiring close storm (e.g. signing key rotation) — expected vs incident
- [ ] **Verify:** Runbook reviewed by an on-call owner

### Task 27: Tenant config flag for rollout

**Files:**
- Modify: tenant config schema in `messaging-provider-webchat-gui`
- Modify: `src/http_ingress/webchat_ws/mod.rs` (gate handler on flag)

- [ ] Add `messaging-webchat-gui.websocket_enabled: bool` (default `false`)
- [ ] When false, the `/stream` endpoint returns 501 (preserves current behaviour)
- [ ] When true, runner-host serves the WS handler
- [ ] **Verify:** Toggle flag in dev tenant config; WS upgrade succeeds with `true`, returns 501 with `false`

### Task 28: Rollout dry-run

- [ ] Enable in Greentic dogfood tenant; soak 48 h
- [ ] Watch metrics: `webchat_ws_connections_active`, push latency p99, disconnect rate
- [ ] Document any unexpected behaviour in runbook
- [ ] **Verify:** No regressions vs polling baseline; push latency < 200 ms p99

---

## Phase I — Stretch (post-merge)

These are tracked but not part of the v1 scope:

- Multi-region active-active backplane (ElastiCache Global Datastore / Memorystore replication)
- PSUBSCRIBE wildcard optimization for high-fan-out tenants
- Separate WS metrics dashboard in Grafana (templates for AWS Managed Grafana, Cloud Monitoring, Azure Monitor)
- Polling deprecation timeline (revisit after 6 months of stable WS in prod)

---

## Risk-mitigation checklist (gate before each phase deploys)

Before promoting from Phase B → C → D → E → F → G → H deployment, verify:

- [ ] All preceding-phase tests pass (`./ci/local_check.sh` green)
- [ ] No new clippy warnings (`-D warnings` strict)
- [ ] No new `unwrap()`/`panic!()` in production paths (per CLAUDE.md global guardrails)
- [ ] Spec §16 risk register reviewed; new risks added if discovered
- [ ] Open questions in spec §18 still open or marked resolved with decision recorded

---

## Effort summary

| Phase | Days | Parallelizable | Owner |
|-------|------|----------------|-------|
| A. Alignment | 1-2 | no (review-bound) | Author → reviewer |
| B. Core WS handler | 5 | partial (Tasks 4/5/6 sequential, 7 parallel) | Implementer |
| C. Redis backplane | 3 | partial | Implementer |
| D. Hardening | 2 | yes (Tasks 12-15 mostly independent) | Subagents |
| E. Observability | 1 | yes | Subagent |
| F. Tests | 3 | partial (unit ‖ integration ‖ chaos) | Subagents |
| G. Cloud configs | 2 | yes (3 clouds independent) | Subagents per cloud |
| H. Ops & rollout | 1 | no | Implementer |

**Critical path: A → B → C → F (multi-replica integration test) ≈ 12 days.** Phases D/E/G can fan out via subagents in parallel with C and F, compressing total wall-clock to ~13 days with subagent dispatch.

---

## References

- Spec: `docs/superpowers/specs/2026-04-30-webchat-directline-websocket-design.md`
- Existing WS handler precedent in repo: none (this is the first long-lived WS in greentic-runner-host)
- Pattern reference (different layer): `greentic-designer/docs/superpowers/specs/2026-04-16-streaming-preview-design.md` — SSE for designer agent panel; not WebSocket but illustrates similar streaming primitives in axum
