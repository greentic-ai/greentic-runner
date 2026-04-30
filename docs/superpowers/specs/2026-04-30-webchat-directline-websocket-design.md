# WebChat Direct Line WebSocket Transport — Design Spec

**Date:** 2026-04-30
**Status:** Draft, pending review
**Related repos:** `greentic-start` (impl host), `greentic-runner`, `greentic-messaging-providers`

---

## 1. Context

The Greentic webchat operator currently exposes the Direct Line v3 protocol over **HTTP polling only**. Clients hit `GET /v3/directline/conversations/{id}/activities?watermark=N` every ~1 s to fetch new bot replies. The server-side handler at `components/messaging-provider-webchat/src/directline/http.rs:80` returns **HTTP 501** for the stream upgrade endpoint:

```rust
["v3", "directline", "conversations", _conv_id, "stream"] => respond_not_implemented(),
```

The Microsoft `botframework-webchat` client library auto-detects this 501 and silently falls back to polling. **No client changes are needed to enable WebSocket** — we only need to implement the server side.

Polling has measurable downsides at production scale:

- **HTTP layer cost:** every cloud LB charges per new-connection rate. AWS ALB caps "new connections" at 25/s per LCU; 1000 users polling every 5 s burns ~8 LCUs ($62/month) versus ~0.34 LCU ($18/month) for WebSocket.
- **Latency floor:** clients see ≥500 ms reply latency from the polling interval, regardless of how fast the bot is.
- **Wasted compute:** polling spins CPU on the runner per request even when there is no new activity.

Platform direction is to ship this **production-ready, not demo-quality**.

A separate, smaller fix (rate limit raised + IP-based bucketing on the token endpoint, client-side guest UUID + token cache) is already implemented on branch `fix/webchat-rate-limit-bucketing` of `greentic-messaging-providers`. That fix continues to be valuable as a **defence-in-depth layer** even after WebSocket ships, since polling remains as a fallback.

---

## 2. Goal

Implement a Direct Line v3 streaming WebSocket endpoint at `GET /v3/directline/conversations/{id}/stream?watermark=N&t=TOKEN` that:

1. Pushes `ActivitySet` frames to connected clients in real time as bots produce replies.
2. Survives multi-instance horizontal scaling without dropping messages between replicas.
3. Drains gracefully on rolling restart so clients reconnect without a perceptible hiccup.
4. Enforces resource limits to prevent a single tenant or IP from exhausting the pool.
5. Emits the metrics needed to alert on connection-pool saturation, push latency, and disconnect storms.

Polling remains supported as a fallback. No client-side change is required: setting the streamUrl in the conversation creation response causes the Microsoft Web Chat library to auto-upgrade.

---

## 3. Sub-project context

| # | Sub-project | Status |
|---|-------------|--------|
| 1 | Webchat rate-limit hardening (server-side rate limit + IP bucketing + Retry-After + JS guest UUID + JS token cache) | Done on `fix/webchat-rate-limit-bucketing` (`greentic-messaging-providers`). Pending review. |
| 2 | **Direct Line WebSocket transport** (this spec) | Design |
| 3 | Multi-instance Redis backplane integration | Folded into sub-project 2 |
| 4 | Cloud-specific deployment hardening (Fargate stopTimeout, Cloud Run timeout, ACA premium ingress) | Folded into sub-project 2 (Section 12 below) |

---

## 4. Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Transport | **WebSocket** (`Sec-WebSocket-Protocol: directline.botframework.com`) | Direct Line standard; native client support in `botframework-webchat`; ~5× cheaper than polling at 1000+ users (per cloud research). |
| WS handler location | **`greentic-start`** HTTP ingress (`src/http_ingress/`, raw hyper), not WASM component | The `/v1/messaging/webchat/{tenant}/...` ingress already lives in `greentic-start` over raw hyper (not axum); `greentic-runner-host` only serves admin/operator routes. WASM `ingest_http` is one-shot request/response and unfit for long-lived WS. WS upgrade uses `hyper::upgrade::on(req)` plus `tokio-tungstenite::WebSocketStream::from_raw_socket` (or the `hyper-tungstenite` helper). Component still owns activity persistence + token verification. |
| Cross-instance fan-out backplane | **Redis Pub/Sub** (managed service per cloud — ElastiCache / Memorystore / Azure Cache for Redis) | Industry standard (Slack, Discord, ASP.NET SignalR). All 3 target clouds offer first-class managed Redis. NATS rejected: greentic operator's `--nats off` default would force new infra; no managed NATS on AWS/GCP/Azure. Sticky sessions alone cannot solve the cross-instance push problem (confirmed across all 3 cloud docs). |
| Backplane abstraction | **Strict Redis, no trait** | Matches CLAUDE.md guideline "don't introduce abstractions beyond what task requires". |
| Frame format | Direct Line `ActivitySet` JSON, watermark-incremented | Microsoft spec; Web Chat library expects this. |
| Keepalive | **Server ping every 25 s** (under all 3 cloud idle-timeout floors) | AWS ALB default 60 s, ACA default ingress 240 s, Cloud Run can drop after request-timeout — 25 s ping is safe for all. |
| Auth | JWT in `?t=TOKEN` query string at upgrade; verify before accepting WS | Direct Line standard. Token already bound to `(env, tenant, conversationId)` via existing `verify_token` in `messaging-provider-webchat/src/directline/jwt.rs`. |
| Token in URL — security mitigation | Rely on existing 30-min TTL + conversationId binding; Web Chat lib re-mints on TTL via `/v3/directline/tokens/refresh` | Per Microsoft Direct Line; standard practice for browser WS auth. |
| Polling compatibility | **Keep polling endpoint** (`GET /v3/directline/conversations/{id}/activities?watermark=N`) operational and fully functional | Hard cutover would break clients in restrictive networks (corporate firewalls that strip WS upgrade). Polling = fallback path. |
| Migration strategy | Server adds WS, client lib auto-detects via response `streamUrl`, no JS change | Microsoft `botframework-webchat` upgrade detection logic handles fallback automatically. |
| Resource limits scope | Per-tenant connection cap, per-IP connection cap, idle timeout, frame size limit, slow-consumer drop | All required for production; sized via tenant config (default sensible). |
| Observability | OpenTelemetry traces + Prometheus metrics + structured logs | Match the existing `tracing`-based observability surface inside `greentic-start` (and the shared `greentic-telemetry` integration). |
| Graceful shutdown | SIGTERM → fail readiness probe → send `1001 Going Away` close frames → 30 s drain → exit 0 | Honors LB connection draining on all 3 clouds; clients auto-reconnect via Web Chat lib reconnect logic. |

---

## 5. Architecture overview

### 5.1 Component placement

```
                         ┌──────────────────────────────┐
                         │   Browser (Web Chat client)   │
                         └──────────────┬───────────────┘
                                        │
                              wss:// upgrade + JWT
                                        │
                ┌──────────────────────────────────────┐
                │  Cloud LB (ALB / Cloud Run / ACA)    │
                │  - terminates TLS                     │
                │  - sticky session (cookie)            │
                └──────────────┬───────────────────────┘
                               │
        ┌──────────────────────┼──────────────────────┐
        │                      │                      │
        ▼                      ▼                      ▼
┌──────────────┐       ┌──────────────┐       ┌──────────────┐
│ Replica 1    │       │ Replica 2    │       │ Replica N    │
│ (greentic-   │       │ (greentic-   │       │ (greentic-   │
│  start hyper)│       │  start hyper)│       │  start hyper)│
│ - WS handler │       │ - WS handler │       │ - WS handler │
│ - WASM pool  │       │ - WASM pool  │       │ - WASM pool  │
│ - Redis sub  │       │ - Redis sub  │       │ - Redis sub  │
└──────┬───────┘       └──────┬───────┘       └──────┬───────┘
       │                      │                      │
       │ PUBLISH conv:{id}    │                      │
       │ on activity emit     │                      │
       ▼                      ▼                      ▼
                 ┌─────────────────────────────────┐
                 │   Redis Pub/Sub (managed)       │
                 │   - ElastiCache Serverless      │
                 │   - Memorystore Standard        │
                 │   - Azure Cache for Redis       │
                 └─────────────────────────────────┘
```

### 5.2 Push flow

1. **Client connects:** `GET /v3/directline/conversations/{id}/stream?watermark=N&t=JWT` → `greentic-start` performs the hyper upgrade to WS via `hyper::upgrade::on(req)` + `tokio-tungstenite::WebSocketStream::from_raw_socket` (or `hyper-tungstenite`).
2. **Auth:** the ingress handler extracts JWT from query, calls existing `verify_token` (signing key from secrets store), confirms `conv` claim matches path conversation ID, confirms `(env, tenant)` claims match the request's resolved context.
3. **Watermark catch-up:** the ingress handler calls `messaging-provider-webchat`'s WASM via the existing `dispatch_http_ingress` flow (same JSON contract as polling) with `watermark=N`. Replays missed activities to the client as a single `ActivitySet` frame.
4. **Subscribe:** the ingress handler issues `SUBSCRIBE webchat:activity:{tenant}:{conv_id}` on the shared Redis client. Stores the `(conv_id, mpsc::Sender<Frame>)` in a per-replica connection registry.
5. **Bot reply path:**
   - Bot or pack flow POSTs an activity to `/v3/directline/conversations/{id}/activities` (HTTP, can land on any replica).
   - WASM component stores activity in state store + bumps watermark (existing logic, unchanged).
   - WASM emits `events: Vec<ChannelMessageEnvelope>` in `HttpOutV1` (existing behaviour).
   - The ingress handler intercepts the `events` field after WASM returns, **publishes** `webchat:activity:{tenant}:{conv_id}` on Redis with the activity payload as the message body.
   - Every replica subscribed to that channel receives the message via its `SUBSCRIBE` connection.
   - Each replica looks up its local connection registry — for any WS sender bound to that `conv_id`, it pushes the frame.
6. **Client disconnect:** the ingress handler removes the entry from the registry; if no other connection on this replica subscribes to the channel, it `UNSUBSCRIBE`s.

### 5.3 Why publish on the producing replica, not from the WASM component

Two options were considered for the publish step:

- **(a) WASM publishes directly:** would require a WIT host API (`pubsub::publish(channel, payload)`) — extension surface change in `greentic-interfaces`, blast radius far beyond webchat.
- **(b) The ingress handler publishes on behalf of WASM:** zero WIT change. After the WASM component returns `HttpOutV1 { events, ... }`, the ingress code iterates events and publishes those that match `provider=messaging-webchat-gui` on the corresponding Redis channel.

**Decision: (b).** Smaller blast radius, no contract change, and matches the existing intercept pattern at `greentic-start/src/ingress_dispatch.rs` (envelope handling already happens there).

---

## 6. Wire protocol

### 6.1 Upgrade

```http
GET /v3/directline/conversations/abc-123/stream?watermark=0&t=eyJ... HTTP/1.1
Host: webchat.example.com
Connection: Upgrade
Upgrade: websocket
Sec-WebSocket-Version: 13
Sec-WebSocket-Key: <browser-generated>
Sec-WebSocket-Protocol: directline.botframework.com
Origin: https://webchat.example.com
```

Server responds `101 Switching Protocols` after token verification. If verification fails, return `401 Unauthorized` (do not upgrade) with the same body shape as the polling 401 path.

### 6.2 Frames sent server → client

All frames are JSON text frames (`opcode 0x1`). Each carries an `ActivitySet`:

```json
{
  "activities": [
    {
      "type": "message",
      "id": "1234abcd",
      "timestamp": "2026-04-30T08:15:30.123Z",
      "from": { "id": "bot", "name": "Bot", "role": "bot" },
      "text": "Here is your research plan.",
      "attachments": [ /* adaptive cards, etc. */ ],
      "watermark": "42"
    }
  ],
  "watermark": "42"
}
```

This shape is identical to the polling `GET /activities` response — same JSON, just delivered over WS instead of HTTP. The Microsoft Web Chat library handles both paths via the same parser.

### 6.3 Frames sent client → server

**None expected on this WS channel.** Direct Line uses the WS only for receive (server → client). Client → server messages still go through HTTP `POST /v3/directline/conversations/{id}/activities`. If the client sends a frame, log a warning and discard.

### 6.4 Keepalive

- **Server:** sends WS ping (`opcode 0x9`) with empty payload every **25 s** of inactivity.
  - Rationale: AWS ALB default idle timeout 60 s, ACA default ingress 240 s, Cloud Run drops at request-timeout. 25 s is safely under all three when raised to the recommended values (Section 12).
- **Client:** Microsoft Web Chat library auto-replies pong; no app code needed.
- If no pong within 10 s of ping, server closes with `1011 Internal Error` and removes registry entry. Client auto-reconnects.

### 6.5 Close codes

| Code | Reason | When |
|------|--------|------|
| `1000` Normal | Client closed | Tab closed, navigation |
| `1001` Going Away | Server shutdown | SIGTERM during deploy / scale-in |
| `1008` Policy Violation | Auth/limits | Token expired mid-connection (rare; we close at TTL); rate limit |
| `1011` Internal Error | Pong timeout / Redis disconnect / pack error | Recoverable from client perspective; reconnect |
| `4000+` (custom) | Tenant-specific | Reserved, not used in v1 |

### 6.6 Watermark semantics

Same as polling: every activity has a monotonic `watermark` (string-encoded integer per Microsoft spec). On upgrade, `?watermark=N` indicates "I have seen up to N, send me everything strictly greater than N." The server's catch-up replay (Section 5.2 step 3) uses this exactly.

---

## 7. Auth flow

### 7.1 Token at upgrade

JWT is passed via `?t=TOKEN` query parameter (Direct Line standard). The ingress handler's WS upgrade flow:

1. Extracts `t` from query.
2. Loads the signing key (from secrets store via the same path WASM uses).
3. Calls `verify_token(&signing_key, &token)` — same function as `messaging-provider-webchat/src/directline/http.rs::verify_token`.
4. Asserts `claims.conv == path conversation_id` (existing logic in `handle_reconnect_conversation`).
5. Asserts `claims.ctx.env == request_env` and `claims.ctx.tenant == request_tenant` (resolved via ingress handler's tenant resolver).
6. If any check fails, return `401` and **do not upgrade**.

### 7.2 Token TTL during connection

Direct Line tokens currently TTL at 30 minutes (`messaging-provider-webchat/src/directline/jwt.rs::TTL_SECONDS = 1800`). Two options for in-connection TTL handling:

- **(a) Close on TTL expiry, force reconnect with new token.** Simple. Microsoft Web Chat library handles this via `/v3/directline/tokens/refresh` (currently 404 in our server — we'll fix as part of this work).
- **(b) Allow connection to continue past TTL.** Less secure; rejected.

**Decision: (a).** When the connection enters its TTL-3-minute window, server sends `1008 Policy Violation` close with a `Sec-WebSocket-Close` reason `"token_expiring"`. Client (Web Chat) refreshes token via existing token endpoint and reconnects with new JWT.

### 7.3 Sec-WebSocket-Protocol negotiation

Client sends `Sec-WebSocket-Protocol: directline.botframework.com`. Server responds with the same in upgrade response. Mismatched protocol → close pre-upgrade with 426 Upgrade Required.

### 7.4 Origin allowlist

Production deployments MUST set an allowlist via tenant config:
```yaml
webchat:
  ws_origin_allowlist:
    - https://webchat.example.com
    - https://*.example.com
```

Origin not on list → return `403 Forbidden` at upgrade. In dev, allowlist is `*` by default (configurable).

---

## 8. Redis backplane design

### 8.1 Connection topology

- **Per replica:** maintain ONE Redis client connection for the SUBSCRIBE side and a separate connection (or pool) for PUBLISH operations. Redis pub/sub mode blocks subscriber connections from running other commands.
- **Redis driver:** `redis-rs` 0.27+ (workspace dependency, async tokio) with `redis::aio::ConnectionManager` for auto-reconnect.
- **TLS:** `rediss://` URL scheme; required for production. Dev override allowed via env var.

### 8.2 Channel naming

```
webchat:activity:{tenant}:{conversation_id}
```

- `tenant` is from JWT `claims.ctx.tenant`.
- `conversation_id` is path-segment from upgrade URL.

Channel name is colon-delimited per Redis convention. No wildcard subscriptions in v1 (PSUBSCRIBE) — each replica subscribes to exactly the conversations its WS clients hold. Trade-off: more SUBSCRIBE/UNSUBSCRIBE traffic, but bounded message fan-out per channel. PSUBSCRIBE could be a future optimization.

### 8.3 Subscription lifecycle

| Event | Action |
|-------|--------|
| First WS for conv X on this replica | `SUBSCRIBE webchat:activity:{tenant}:{X}` |
| Additional WS for same conv X | No-op (already subscribed) |
| Last WS for conv X disconnects | `UNSUBSCRIBE webchat:activity:{tenant}:{X}` |
| Replica startup | No subscriptions; populate on demand |
| Replica shutdown (SIGTERM) | Drain WS first; `UNSUBSCRIBE` is implicit on connection close |
| Redis disconnect | `ConnectionManager` auto-reconnects; replica RE-SUBSCRIBEs all active channels |

A per-replica `HashMap<String, RefCount>` tracks which channels are currently subscribed and how many local WS connections depend on each.

### 8.4 Publish flow

After WASM `ingest_http` returns from `POST /v3/directline/conversations/{id}/activities`:

```rust
for envelope in http_out.events.iter()
    .filter(|e| e.metadata.get("provider") == Some(&"messaging-webchat-gui".into()))
{
    let channel = format!(
        "webchat:activity:{}:{}",
        envelope.metadata.get("tenant").unwrap_or(&"default".into()),
        conversation_id
    );
    redis.publish(&channel, &serde_json::to_vec(envelope)?).await?;
}
```

Failure mode: if Redis publish fails (transient), **fall back to writing to the local registry only**. Connections on other replicas will not receive the activity, but the polling fallback path keeps working — clients eventually catch up via watermark.

### 8.5 Message format on Redis

Redis pub/sub messages carry the activity as JSON bytes. Each subscriber decodes and re-serializes to the WS frame format. We do NOT include the `ActivitySet` wrapper in the Redis message — that's added per-WS-connection at delivery time, since each connection has its own watermark progression.

### 8.6 Redis sizing

- 1000 concurrent users → ~1000 active channels (worst case 1 conv per user). Redis handles 100k+ channels easily on the smallest managed tier.
- Throughput: chat traffic is sparse — 1-5 messages/min/user typical. ElastiCache Serverless minimum (~$60/month) far oversized.
- Cost: AWS ElastiCache Serverless ~$60/month, GCP Memorystore Basic 1 GB ~$35/month, Azure Cache for Redis Standard C0 ~$45/month.

---

## 9. Resource limits

All limits configurable via tenant config; defaults sensible for 1000 concurrent users per replica.

| Limit | Default | Configurable | Enforcement |
|-------|---------|--------------|-------------|
| Max WS connections per replica | 5000 | yes (per tenant) | At upgrade — return `503 Service Unavailable` with `Retry-After` |
| Max WS connections per IP | 50 | yes | At upgrade — return `429 Too Many Requests` |
| Max WS connections per tenant | 10000 | yes | At upgrade — return `503` |
| Max idle time (no pong) | 35 s | yes | Close `1011` |
| Max frame size (server → client) | 256 KiB | yes | If activity exceeds, omit attachments and add a synthetic `truncated: true` flag; client re-fetches via polling |
| Slow-consumer threshold | 4 KiB unflushed buffer | yes | Drop the connection (`1011`) — do NOT block the broadcast channel |
| Max conn lifetime | 30 min (matches token TTL) | no | Close `1008 token_expiring` |
| Max upgrade rate per IP | 5 / min | yes | `429` |

The slow-consumer rule is critical: a Tokio `mpsc` channel must be **bounded**, and the publisher MUST drop the slow client rather than block, otherwise one slow browser stalls the broadcast for everyone.

---

## 10. Observability

### 10.1 Prometheus metrics

```
# HELP webchat_ws_connections_active Active WebSocket connections
# TYPE webchat_ws_connections_active gauge
webchat_ws_connections_active{tenant="demo", env="prod"} 1273

# HELP webchat_ws_connections_total Total WS connections accepted since startup
# TYPE webchat_ws_connections_total counter
webchat_ws_connections_total{tenant="demo", env="prod"} 18432

# HELP webchat_ws_disconnects_total Disconnects, by reason
# TYPE webchat_ws_disconnects_total counter
webchat_ws_disconnects_total{tenant="demo", reason="client_close"} 17800
webchat_ws_disconnects_total{tenant="demo", reason="server_shutdown"} 23
webchat_ws_disconnects_total{tenant="demo", reason="pong_timeout"} 412
webchat_ws_disconnects_total{tenant="demo", reason="slow_consumer"} 8
webchat_ws_disconnects_total{tenant="demo", reason="token_expiring"} 1140

# HELP webchat_ws_push_latency_seconds Time from activity emit to WS frame flushed
# TYPE webchat_ws_push_latency_seconds histogram
webchat_ws_push_latency_seconds_bucket{tenant="demo", le="0.01"} 14000
webchat_ws_push_latency_seconds_bucket{tenant="demo", le="0.05"} 17800
webchat_ws_push_latency_seconds_bucket{tenant="demo", le="0.5"} 18400
webchat_ws_push_latency_seconds_count{tenant="demo"} 18432
webchat_ws_push_latency_seconds_sum{tenant="demo"} 192.4

# HELP webchat_ws_dropped_frames_total Frames dropped due to limits
# TYPE webchat_ws_dropped_frames_total counter
webchat_ws_dropped_frames_total{tenant="demo", reason="oversized"} 12
webchat_ws_dropped_frames_total{tenant="demo", reason="slow_consumer"} 8

# HELP webchat_ws_redis_subscriptions Active Redis channel subscriptions on this replica
# TYPE webchat_ws_redis_subscriptions gauge
webchat_ws_redis_subscriptions 412

# HELP webchat_ws_redis_publish_total Redis PUBLISH calls
# TYPE webchat_ws_redis_publish_total counter
webchat_ws_redis_publish_total{result="ok"} 22341
webchat_ws_redis_publish_total{result="error"} 14
```

### 10.2 Tracing

Each WS connection gets a `tracing::Span` with `conversation_id`, `tenant`, `env`, `replica_id`. Span enters at upgrade, exits at close. Spans for individual frame pushes use `Span::record` to correlate with publish events from other replicas.

Push latency (publish → flush) is the key SLO metric. Target p99 < 200 ms.

### 10.3 Structured logs

```
INFO ws_connection_open conv_id=abc-123 tenant=demo replica=r2 origin=https://chat.x.io
INFO ws_connection_close conv_id=abc-123 tenant=demo replica=r2 reason=client_close duration_ms=185432 frames_sent=42
WARN ws_pong_timeout conv_id=abc-123 tenant=demo replica=r2 last_pong_age_ms=11200
ERROR ws_redis_publish_failed channel=webchat:activity:demo:abc-123 err="connection refused" fallback=local_only
```

Span context propagation: when WASM `POST /activities` returns, the ingress handler extracts the trace context from `HttpOutV1` headers (existing pattern) and uses it to correlate the publish event with the originating activity.

---

## 11. Graceful shutdown

### 11.1 Sequence

1. Operator process receives `SIGTERM`.
2. **Readiness probe flips to fail** — LB stops sending new upgrade requests to this replica.
3. **Wait `pre_drain_grace` (default 5 s)** for in-flight upgrade requests to complete or fail.
4. **Iterate WS registry**: send `1001 Going Away` close frame to every connected client.
5. **Wait `drain_timeout` (default 30 s)** for clients to close. Microsoft Web Chat library auto-reconnects within ~1 s of receiving `1001`.
6. **Force-close** any remaining connections with `1011`.
7. **Drop Redis SUBSCRIBE** connection.
8. **Exit 0**.

The drain budget is bounded by the cloud's hard ceiling:

- AWS Fargate `stopTimeout` cap: **120 s** (max). Runner config `drain_timeout` should be ≤ 110 s with 10 s margin.
- GCP Cloud Run: graceful shutdown gives up to `--timeout` value (recommend 3600 s); set runner config to 60 s explicitly.
- Azure Container Apps: `terminationGracePeriodSeconds` configurable 0-3600; default 30 s. Set to 1200 s for premium ingress with explicit drain timeout 60 s.

### 11.2 Configuration

```yaml
runner:
  shutdown:
    pre_drain_grace_seconds: 5
    drain_timeout_seconds: 30
```

### 11.3 In-flight HTTP requests

Existing hyper-based ingress shutdown in `greentic-start` already handles non-WS requests via the broadcast `oneshot` shutdown signal. WS drain is layered on top via the WS registry — see implementation plan task list.

---

## 12. Cloud deployment configurations

Configuration matrix derived from cloud provider research (April 2026). Each cloud has distinct knobs that must be tuned together for WS to work correctly.

### 12.1 AWS (ECS Fargate + ALB)

**ALB:**
```hcl
resource "aws_lb" "webchat" {
  name               = "greentic-webchat-alb"
  load_balancer_type = "application"
  idle_timeout       = 600   # 10 min; 25 s app ping is well under this
}
```

**Target group:**
```hcl
resource "aws_lb_target_group" "webchat" {
  name                 = "webchat-tg"
  port                 = 8080
  protocol             = "HTTP"
  target_type          = "ip"   # Fargate awsvpc
  deregistration_delay = 110    # under Fargate stopTimeout cap, 10 s margin
  stickiness {
    type            = "app_cookie"
    cookie_name     = "GTC_AFFINITY"
    cookie_duration = 86400
    enabled         = true
  }
}
```

**ECS task:**
```json
{
  "family": "webchat-operator",
  "containerDefinitions": [{
    "name": "webchat",
    "image": "ghcr.io/greenticai/greentic-runner:0.5.x",
    "stopTimeout": 110,
    "essential": true,
    "environment": [
      { "name": "REDIS_URL", "value": "rediss://greentic-webchat.use1.cache.amazonaws.com:6379" }
    ]
  }]
}
```

**Auto-scaling (Application Auto Scaling target tracking on custom CloudWatch metric):**
```yaml
metric:    Greentic/Webchat WebsocketConnectionsPerTask
target:    500
namespace: Greentic/Webchat
scale_out_cooldown: 60
scale_in_cooldown:  300
```

Replica publishes `WebsocketConnectionsPerTask` every 60 s; scheduled Lambda computes derived metric.

**Backplane: ElastiCache Serverless for Redis.** Connect via VPC; security group permits :6379 from the ECS service security group only.

### 12.2 GCP (Cloud Run)

```bash
gcloud run deploy greentic-webchat \
  --image=europe-west1-docker.pkg.dev/PROJECT/greentic/webchat:0.5.x \
  --region=europe-west1 \
  --execution-environment=gen2 \
  --concurrency=250 \
  --cpu=1 --memory=512Mi \
  --min-instances=2 \
  --max-instances=20 \
  --timeout=3600 \
  --no-cpu-throttling \
  --session-affinity \
  --network=greentic-vpc \
  --subnet=greentic-cr-subnet \
  --vpc-egress=private-ranges-only \
  --set-env-vars=REDIS_HOST=10.x.x.x,REDIS_PORT=6379 \
  --service-account=webchat@PROJECT.iam.gserviceaccount.com
```

**Critical Cloud Run-specific knobs:**
- Do NOT enable `--use-http2` (end-to-end HTTP/2 breaks WS upgrade in Web Chat lib).
- `--timeout=3600` is the maximum (60 min). Server-side ~55-min token expiry forces a reconnect well before this hard cap.
- `--concurrency=250` — single WS = 1 concurrent request, default 80 too tight.
- `--min-instances=2` — avoid cold-start drops on first morning chat.
- `--no-cpu-throttling` — instance-based billing; required for steady WS load (request-based billing meters CPU only when "active" but a continuous WS keeps the request open the whole time, so request-based billing doesn't actually save).

**Backplane: Memorystore Redis.** Direct VPC egress; same VPC as Cloud Run service; Standard tier (HA) for production.

### 12.3 Azure (Container Apps)

**Premium ingress** (required for >4 min WS idle):
```bash
az containerapp env premium-ingress add \
  --resource-group rg-greentic \
  --name env-greentic \
  --workload-profile-name Ingress-D4 \
  --termination-grace-period 1200 \
  --request-idle-timeout 30
```

**Container app (Bicep):**
```bicep
resource webchat 'Microsoft.App/containerApps@2024-03-01' = {
  name: 'webchat'
  properties: {
    configuration: {
      ingress: {
        external: true
        targetPort: 8080
        transport: 'auto'
        stickySessions: { affinity: 'sticky' }
      }
    }
    template: {
      terminationGracePeriodSeconds: 110   // matches WS drain budget
      containers: [{
        name: 'webchat'
        image: 'ghcr.io/greenticai/greentic-runner:0.5.x'
        env: [
          { name: 'REDIS_URL', value: 'rediss://greentic-webchat.redis.cache.windows.net:6380' }
        ]
        resources: { cpu: 1, memory: '2Gi' }
      }]
      scale: {
        minReplicas: 2
        maxReplicas: 30
        rules: [
          { name: 'ws-conn',
            http: { metadata: { concurrentRequests: '200' } } },
          { name: 'mem-guard',
            custom: {
              type: 'memory',
              metadata: { type: 'Utilization', value: '70' }
            } }
        ]
      }
    }
  }
}
```

**Critical ACA knobs:**
- Default ingress idle 240 s — premium ingress required for prod.
- `stickySessions: sticky` requires single-revision mode (no traffic-split blue/green for this app).
- Cookie name undocumented — don't depend on it in app code.

**Backplane: Azure Cache for Redis Standard.** VNet integrated; same private network as Container Apps environment.

### 12.4 Local development

Embedded Redis via `testcontainers` (Rust crate) for tests; for `cargo run --bin greentic-runner` against a local stack, recommend `redis:7-alpine` Docker container:
```bash
docker run -d --name greentic-redis -p 6379:6379 redis:7-alpine
REDIS_URL=redis://127.0.0.1:6379 cargo run --bin greentic-runner
```

For single-instance local dev, Redis can be skipped — ingress handler detects empty `REDIS_URL` and falls back to in-memory broadcast (works for one replica only). This **MUST NOT** be used in production; runner emits a warning log on startup.

---

## 13. Migration plan

### 13.1 Compatibility matrix

| Client config | Server WS impl | Result |
|---------------|----------------|--------|
| `webSocket: true` (default) | Polling-only (current) | Web Chat tries WS upgrade → 501 → falls back to polling. Works. |
| `webSocket: true` | WS implemented (this spec) | Web Chat upgrades successfully. New behaviour. |
| `webSocket: false` | Either | Polling. Works. |

No client config change required — Microsoft Web Chat detects via `streamUrl` field in conversation creation response (which the server already returns).

### 13.2 Rollout strategy

1. **Deploy with WS off-by-default** behind a tenant config flag `messaging-webchat-gui.websocket_enabled: false`. When false, the `/stream` endpoint continues to return 501. Polling unaffected.
2. **Enable per-tenant in staging** for validation. Soak 24-48 h. Watch metrics: `webchat_ws_connections_active`, push latency, disconnect rate.
3. **Enable in dogfood tenant** (Greentic-internal). Soak 48 h.
4. **Enable per-customer-tenant** progressively, starting with low-traffic tenants. Polling stays as fallback.
5. **No global on-by-default** until 4+ weeks of dogfood without regressions.

### 13.3 Polling deprecation

Polling endpoint stays operational indefinitely. Deprecation is **not** in scope for this work — to be revisited only after WS proves stable across all customer tenants (probably 6+ months).

### 13.4 Rate-limit fix interaction

Sub-project 1 (rate limit + JS guest UUID + token cache) provides defence-in-depth at the polling/HTTP layer. After WebSocket ships:
- The token endpoint rate limit still applies — clients still call `/token` to get a JWT for WS upgrade.
- JS token cache reduces /token calls regardless of transport.
- IP bucketing still useful if WS is forced to fall back due to firewall.

Sub-project 1 should land **before** sub-project 2 deploys to staging, so the staging baseline already has the hardened HTTP path.

---

## 14. Failure modes

| Failure | Detection | Mitigation |
|---------|-----------|------------|
| Redis cluster unreachable | `redis::aio` connection error on PUBLISH | Log + metric increment; deliver activity locally only; client on other replicas catches up via polling fallback. Replica retries Redis connect via `ConnectionManager`. |
| Replica OOM (too many connections) | `webchat_ws_connections_active` exceeds limit | Per-replica connection cap returns 503 at upgrade; LB skips to other replica. Auto-scaling spins another replica. |
| Slow consumer (mobile on EDGE) | `mpsc` send returns full | Drop connection with `1011`; metric `slow_consumer`. Client reconnects on better network. |
| Token expires mid-connection | Server timer at TTL-3min | Send `1008 token_expiring`; client refreshes token + reconnects. |
| Pong timeout | No pong within 10 s of ping | Close `1011`; client reconnects. |
| WASM component crashes | ingress handler catches panic | WS path unaffected (the hyper ingress layer is upstream of WASM). Returns appropriate HTTP error to ingest path. |
| Replica crash / scale-in | `1001 Going Away` close on shutdown OR connection reset | Web Chat lib auto-reconnects to a different replica. Watermark catch-up replays missed activities. |
| Network partition between replica and Redis | Redis subscriber connection drops | `ConnectionManager` auto-reconnects + RE-SUBSCRIBEs. During partition, push latency increases for cross-replica traffic; same-replica traffic unaffected. |
| Conversation watermark gap (replica missed messages during Redis outage) | Watermark in stored state differs from incoming | On reconnect, ingress handler runs catch-up replay (Section 5.2 step 3) — same path as initial upgrade. No special case. |
| Origin spoof | Allowlist check at upgrade | 403 Forbidden; metric `webchat_ws_origin_rejected_total`. |
| DOS via mass upgrade attempts | Per-IP upgrade rate limit | 429; metric `webchat_ws_upgrade_rate_limited_total`. |

---

## 15. Tests scope

### 15.1 Unit

- Token verification at upgrade (positive + negative cases)
- Watermark catch-up replay (empty, partial, full)
- Frame format serialization
- Subscription registry add/remove/refcount
- Slow-consumer detection
- Pong timeout state machine

Target: 100% coverage of the new `greentic-start/src/http_ingress/webchat_ws.rs` module (or whatever the final filename is).

### 15.2 Integration

- End-to-end: client connects → bot replies → client receives frame
- Multi-replica via two in-process `greentic-start` HTTP ingress instances + embedded Redis (testcontainers): client A on replica 1, bot reply produced via replica 2's HTTP path → client A receives the frame within 200 ms
- Graceful shutdown drains all WS within budget
- Token expiry triggers `1008 token_expiring`
- Origin allowlist rejects spoof
- Per-IP upgrade rate limit triggers 429

### 15.3 Load

- 10,000 concurrent WS connections on a single replica
- Sustained 100 messages/sec across all conversations for 30 min
- Connection churn: 10% reconnect every minute for 30 min
- Watermark catch-up: simulate 1000 missed messages on reconnect, verify all delivered in order

Tooling: `oha` (HTTP load) for upgrade churn; custom Rust harness using `tokio-tungstenite` for sustained WS traffic; chaos via `tc qdisc` for network packet loss.

### 15.4 Chaos

- Kill Redis pod mid-traffic; verify replicas recover and continue serving (fall back to local-only push during outage; resume cross-replica when Redis returns)
- Force replica restart with active connections; verify clients reconnect within 5 s
- Slow-network client (1 KB/s); verify slow-consumer drop kicks in
- Cross-region partition; verify both regions remain functional independently

### 15.5 Conformance

The Microsoft Direct Line conformance suite is referenced from the Web Chat client, not server-side. We mirror its expectations via integration tests using the actual `botframework-webchat` library in a headless Chromium under Playwright (existing greentic-e2e pattern).

---

## 16. Risk register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Cloud LB idle timeout drops WS unexpectedly | Medium | Medium | 25 s server ping; idle timeout config docs per cloud (Section 12) |
| Redis pricing surprise at scale | Low | Low | Pub/sub volume scales linearly with bot reply volume, not connection count; ElastiCache Serverless minimum is far oversized for 1k users |
| `redis-rs` API churn between versions | Low | Low | Pin minor version; depend on stable `aio::ConnectionManager` API |
| Web Chat library version regression breaks our WS handshake | Low | High | Pin `botframework-webchat` version in `webchat-gui` pack; integration test against pinned version in CI |
| Multi-region deployment requires Redis cross-region replication | Low | Medium | Out of scope for v1; documented as future work. v1 is single-region per replica pool. |
| Sticky session cookie break on session continuation | Medium | Low | Best-effort cookie + Redis backplane covers the gap; design assumes stickiness is optimization, not correctness |
| Fargate 120s drain ceiling insufficient for very long-lived sessions | Low | Low | 30 min token TTL forces reconnect well before any deploy event; clients absorb the reconnect transparently |
| Operator binary size bloat from `redis-rs` | Low | Low | Feature-gated under workspace feature `webchat-ws`; build that excludes feature stays unchanged |

---

## 17. Out of scope

- **Polling deprecation/removal:** explicitly out. Polling remains a supported transport indefinitely.
- **Multi-region active-active backplane:** v1 is single-region per replica pool. Geo-redundant Redis replication (e.g., ElastiCache Global Datastore) is future work.
- **Server-initiated activities (push without prior client message):** out. Direct Line v3 spec requires client to initiate the conversation.
- **WS subprotocol negotiation beyond `directline.botframework.com`:** out. No custom protocols.
- **PSUBSCRIBE wildcard subscriptions:** considered for future optimization if SUBSCRIBE/UNSUBSCRIBE traffic becomes a hot path.
- **Custom backplane drivers (NATS, in-memory clustering):** explicitly rejected. Strict Redis only.
- **JS client changes:** none required. Microsoft Web Chat library handles transport selection.
- **`runtime-bootstrap.js` modifications for WS:** none — Microsoft library negotiates transport directly. Existing JS guest_id + token cache fix continues to apply at the HTTP layer.

---

## 18. Open questions for stakeholder review

1. **Single-region vs multi-region target:** v1 is single-region. Is multi-region active-active required within 6 months? If yes, design needs additional spec (Redis Global Datastore / cross-region pub/sub pattern).
2. **Rollout cadence:** dogfood tenant first, or direct to internal staging? Recommended: dogfood first.
3. **Origin allowlist policy:** strict per-tenant config from day 1, or wildcard during rollout with hardening later? Recommended: wildcard in dev, strict from staging onward.
4. **Sub-project 1 (rate-limit fix on `fix/webchat-rate-limit-bucketing` of `messaging-providers`) merge timing:** this spec assumes that branch lands first. Confirm or sequence both together.
5. **Premium ingress on Azure (~2× ingress D4 nodes):** confirmed acceptable cost increment for Azure customers? If not, strict 4-min idle timeout limits keepalive ping cadence to ≤2 min.
6. **Redis tier per cloud:** Standard/HA from day 1 (recommended) or Basic during rollout?

---

## 19. References

### Direct Line protocol

- [Microsoft — Direct Line API 3.0 receive activities (WebSocket)](https://learn.microsoft.com/en-us/azure/bot-service/rest-api/bot-framework-rest-direct-line-3-0-receive-activities?view=azure-bot-service-4.0)
- [Microsoft — Direct Line authentication](https://learn.microsoft.com/en-us/azure/bot-service/rest-api/bot-framework-rest-direct-line-3-0-authentication?view=azure-bot-service-4.0)

### AWS

- [AWS docs — Application Load Balancers (WebSockets)](https://docs.aws.amazon.com/elasticloadbalancing/latest/application/application-load-balancers.html)
- [AWS docs — Sticky sessions for ALB](https://docs.aws.amazon.com/elasticloadbalancing/latest/application/sticky-sessions.html)
- [AWS docs — Task definition parameters (`stopTimeout`)](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/task_definition_parameters.html)
- [AWS Containers Blog — Graceful shutdowns with ECS](https://aws.amazon.com/blogs/containers/graceful-shutdowns-with-ecs/)
- [AWS Containers Blog — Autoscaling on custom CloudWatch metrics](https://aws.amazon.com/blogs/containers/autoscaling-amazon-ecs-services-based-on-custom-cloudwatch-and-prometheus-metrics/)
- [AWS — Elastic Load Balancing pricing](https://aws.amazon.com/elasticloadbalancing/pricing/)

### GCP

- [GCP — Using WebSockets with Cloud Run](https://docs.cloud.google.com/run/docs/triggering/websockets)
- [GCP — Configuring request timeout](https://docs.cloud.google.com/run/docs/configuring/request-timeout)
- [GCP — Set session affinity](https://docs.cloud.google.com/run/docs/configuring/session-affinity)
- [GCP — Container runtime contract (SIGTERM)](https://docs.cloud.google.com/run/docs/container-contract)
- [GCP — Connect to Memorystore Redis from Cloud Run](https://docs.cloud.google.com/memorystore/docs/redis/connect-redis-instance-cloud-run)
- [GCP — Cloud Run pricing](https://cloud.google.com/run/pricing)

### Azure

- [Azure — Container Apps ingress (protocol types)](https://learn.microsoft.com/azure/container-apps/ingress-overview#protocol-types)
- [Azure — Premium ingress configuration](https://learn.microsoft.com/azure/container-apps/premium-ingress)
- [Azure — Sticky sessions](https://learn.microsoft.com/azure/container-apps/sticky-sessions)
- [Azure — Application lifecycle management (shutdown)](https://learn.microsoft.com/azure/container-apps/application-lifecycle-management#shutdown)
- [Azure — Scale apps (HTTP rule)](https://learn.microsoft.com/azure/container-apps/scale-app#http)
- [Azure — Container Apps billing](https://learn.microsoft.com/azure/container-apps/billing)

### Patterns

- [Ably — Scaling Pub/Sub with WebSockets and Redis](https://ably.com/blog/scaling-pub-sub-with-websockets-and-redis)
- [ASP.NET Core — SignalR scaling and Redis backplane](https://learn.microsoft.com/aspnet/core/signalr/scale?view=aspnetcore-10.0)

### Greentic internal

- `greentic-messaging-providers/components/messaging-provider-webchat/src/directline/http.rs` (current Direct Line server)
- `greentic-messaging-providers/components/messaging-provider-webchat/src/directline/jwt.rs` (token signing/verification)
- `greentic-start/src/http_ingress/` (hyper-based HTTP ingress — target for WS handler)
- `greentic-start/src/http_routes.rs` (route table for `/v1/messaging/webchat/{tenant}/v3/directline/{path*}`)
- `greentic-start/src/ingress_dispatch.rs` (existing intercept point for `HttpOutV1.events` — extension target for the Redis publish hook)
- `greentic-runner-host/src/http/` (axum admin/operator routes — separate from customer ingress)
- `greentic-types/src/messaging/universal_dto.rs` (`HttpInV1`, `HttpOutV1`, `ChannelMessageEnvelope`)
