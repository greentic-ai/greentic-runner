# Agentic Worker Scale-to-Zero — GKE + KEDA + JetStream Hand-Off

This runbook covers the production infrastructure for the `aw-serve` out-of-process
agentic dispatch path (`agentic.call` flow node). The front-door runner offloads
compute to a dedicated `aw-serve` Deployment that scales from zero based on
JetStream consumer lag, driven by KEDA.

---

## 1. NATS JetStream: `greentic-agentic` Stream

### Stream configuration

```
Name:              greentic-agentic
Subjects:          [ "greentic.agentic.request.v1" ]
Retention:         WorkQueuePolicy        # messages deleted on explicit ACK
Storage:           File                   # survives pod restarts
Replicas:          3                      # production minimum; 1 is fine for dev
MaxAge:            24h                    # dead-letter safety net
MaxMsgs:           -1                     # unlimited
MaxBytes:          -1                     # unlimited
Discard:           Old
```

### Durable pull consumer

```
Name:             agentic-workers
Durable:          agentic-workers
Deliver Policy:   All
Ack Policy:       Explicit               # mandatory for WorkQueue
Max Deliver:      5                      # 5 attempts before abandoning
Ack Wait:         300s                   # generous for LLM-heavy steps
Max Waiting:      512                    # concurrent pull requests cap
Filter Subject:   greentic.agentic.request.v1
```

The consumer is auto-created on first `aw-serve` startup via `ensure_consumer`
(`aw-event-bridge`). In production you should pre-create it so KEDA can read the
pending-message count before any `aw-serve` pods exist.

**CLI to pre-create (nats CLI or Terraform NATS provider):**

```sh
nats stream add greentic-agentic \
  --subjects "greentic.agentic.request.v1" \
  --retention workqueue \
  --storage file \
  --replicas 3 \
  --max-age 24h \
  --discard old

nats consumer add greentic-agentic agentic-workers \
  --deliver all \
  --ack explicit \
  --max-deliver 5 \
  --ack-wait 300s \
  --filter "greentic.agentic.request.v1" \
  --pull
```

**Replication for production:** use `--replicas 3` on a 3-node JetStream cluster.
For single-AZ dev clusters `--replicas 1` is acceptable.

---

## 2. GKE Deployment: `aw-serve` (scale-to-zero capable)

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: aw-serve
  namespace: greentic
spec:
  replicas: 0          # KEDA manages replica count; start at 0
  selector:
    matchLabels:
      app: aw-serve
  template:
    metadata:
      labels:
        app: aw-serve
    spec:
      containers:
        - name: aw-serve
          image: registry.greentic.cloud/aw-serve:latest
          resources:
            requests:
              cpu: "500m"
              memory: "512Mi"
            limits:
              cpu: "2"
              memory: "2Gi"
          env:
            - name: GREENTIC_EVENTS_NATS_URL
              valueFrom:
                secretKeyRef:
                  name: greentic-nats
                  key: url
            - name: GREENTIC_AW_REDIS_URL
              valueFrom:
                secretKeyRef:
                  name: greentic-redis
                  key: url
            - name: GREENTIC_LLM_API_KEY
              valueFrom:
                secretKeyRef:
                  name: greentic-llm
                  key: api-key
            - name: GREENTIC_AGENT_MANIFESTS_DIR
              value: /etc/greentic/agents
            - name: GREENTIC_AW_JETSTREAM
              value: "on"                      # default; explicit for clarity
            - name: GREENTIC_AW_WARM_PACKS
              value: "greeter,triage,support"  # comma-separated pack IDs to pre-warm
          volumeMounts:
            - name: agent-manifests
              mountPath: /etc/greentic/agents
              readOnly: true
            - name: cwasm-cache
              mountPath: /var/cache/greentic/cwasm
      volumes:
        - name: agent-manifests
          configMap:
            name: agent-manifests
        - name: cwasm-cache
          emptyDir: {}          # pre-populated at image build time (see §5)
```

**Key env variables:**

| Variable | Purpose |
|---|---|
| `GREENTIC_EVENTS_NATS_URL` | NATS connection URL (JetStream-enabled server) |
| `GREENTIC_AW_REDIS_URL` | Redis for agentic-worker state store |
| `GREENTIC_LLM_API_KEY` | LLM provider API key |
| `GREENTIC_AGENT_MANIFESTS_DIR` | Dir of `<agent_id>.json` `AgentConfig` files |
| `GREENTIC_AW_JETSTREAM` | `on` (default) or `off` for legacy core-NATS |
| `GREENTIC_AW_WARM_PACKS` | Comma-separated pack IDs to warm on startup |
| `CWASM_CACHE_DIR` | Override cwasm cache path (default: `./cwasm-cache`) |

---

## 3. KEDA ScaledObject: JetStream Consumer Scaler

```yaml
apiVersion: keda.sh/v1alpha1
kind: ScaledObject
metadata:
  name: aw-serve-scaledobject
  namespace: greentic
spec:
  scaleTargetRef:
    name: aw-serve

  # Scale to zero when queue is empty.
  minReplicaCount: 0
  maxReplicaCount: 10

  # Time KEDA waits (with empty queue) before scaling back to zero.
  # 300 s gives in-flight sessions time to complete before idle scale-down.
  cooldownPeriod: 300

  # Polling interval for the NATS JetStream lag metric (seconds).
  pollingInterval: 15

  triggers:
    - type: nats-jetstream
      metadata:
        # NATS monitoring endpoint (not the NATS client URL).
        # Point to any single NATS server in the cluster.
        natsServerMonitoringEndpoint: "nats-monitor.greentic.svc.cluster.local:8222"
        account: "$G"                         # default account; adjust for multi-tenant NATS
        stream: "greentic-agentic"
        consumer: "agentic-workers"

        # Scale 1 replica per N pending messages.
        lagThreshold: "5"

        # At least N messages pending before scaling from zero.
        # Prevents spurious scale-up from transient single messages.
        activationLagThreshold: "1"
```

**Tuning guidance:**

- `lagThreshold: "5"` — 1 pod per 5 queued requests; adjust to match your P99 step
  latency budget.
- `cooldownPeriod: 300` — generous for LLM steps; lower to `60` for fast
  tool-only agents.
- `activationLagThreshold: "1"` — scale up on the first queued message. Set to
  `"2"` if you see unwanted bursts from health-check messages.
- `maxReplicaCount: 10` — hard ceiling; each pod is a full `AgentRuntime` with
  Redis state, so keep within your Redis connection budget.

---

## 4. Front-Door Runner: NATS Dispatch Configuration

The front-door `greentic-runner` (HTTP ingress) must NOT scale to zero — it holds
long-lived WebSocket connections (WebChat, Telegram long-poll, etc.).

```
minScale >= 1      # on Cloud Run / GKE HPA; never zero
```

To offload `dw.agent` compute to `aw-serve`, set:

```sh
GREENTIC_AW_DISPATCH=nats
GREENTIC_EVENTS_NATS_URL=nats://nats.greentic.svc.cluster.local:4222
```

**Warning:** if `GREENTIC_EVENTS_NATS_URL` is unset and `GREENTIC_AW_DISPATCH=nats`
is set, every `dw.agent` flow node will fail at dispatch time. The runner logs a
startup warning when `GREENTIC_AW_DISPATCH=nats` is configured but
`GREENTIC_EVENTS_NATS_URL` is absent. Verify both are present in the runner
Deployment env before enabling the NATS path.

```yaml
# Front-door runner Deployment fragment
env:
  - name: GREENTIC_AW_DISPATCH
    value: "nats"
  - name: GREENTIC_EVENTS_NATS_URL
    valueFrom:
      secretKeyRef:
        name: greentic-nats
        key: url
```

---

## 5. cwasm Bake: Pre-Populated Image for Fast Cold Starts

Wasmtime compilation of `.cwasm` files dominates cold-start latency. Build the
`aw-serve` image with the cwasm cache pre-populated so scale-up is near-instant.

**Dockerfile snippet:**

```dockerfile
FROM rust:1.95 AS builder
WORKDIR /build
COPY . .
RUN cargo build -p greentic-aw-runtime --features serve,test-mock --release

# ── warm stage: compile agent packs into cwasm ──
FROM builder AS warmer
ENV CWASM_CACHE_DIR=/cwasm-bake
# Run the aw-serve binary in warm-only mode to pre-compile all listed packs.
# GREENTIC_AW_WARM_PACKS lists the packs to bake (must be reachable from the
# build environment; use a local pack registry or mount packs as files).
RUN AW_SERVE_WARM_ONLY=1 \
    GREENTIC_AW_WARM_PACKS="greeter,triage,support" \
    CWASM_CACHE_DIR=/cwasm-bake \
    ./target/release/aw-serve || true  # non-zero exit if packs not reachable; still copies cache

# ── final image ──
FROM debian:bookworm-slim
COPY --from=builder /build/target/release/aw-serve /usr/local/bin/aw-serve
COPY --from=warmer  /cwasm-bake /var/cache/greentic/cwasm
ENV CWASM_CACHE_DIR=/var/cache/greentic/cwasm
ENTRYPOINT ["/usr/local/bin/aw-serve"]
```

**Rebuild cadence:** rebuild the image whenever agent pack content changes (new
WASM, updated tool list). The cwasm cache is keyed by component digest, so stale
entries are harmless (they will be recompiled on first use) but waste image space.

Set `GREENTIC_AW_WARM_PACKS` in the Deployment env to the same comma-separated
pack IDs used in the image bake; `warm_on_start` logs which packs are targeted at
startup so operators can confirm the env is set correctly.

---

## 6. Follow-Up: Wire `RedisDispatchLedger` for Idempotency

The current `RuntimeAgentDispatchInvoker` defaults to `NoopDispatchLedger`, which
means JetStream redeliveries will re-run the LLM step. To activate
dispatch-level idempotency in production:

1. In `greentic-runner-host::runner::agent_node::build_agent_runtime`, construct a
   `greentic_aw_runtime::RedisDispatchLedger` using the existing Redis
   `ConnectionManager` available in that scope.
2. Wire it via `RuntimeAgentDispatchInvoker::with_ledger(runtime, ledger)` inside
   `greentic_aw_runtime::serve::serve()` (or pass it down from `build_agent_runtime`
   through `serve_agentic`).
3. The TTL for ledger entries should exceed `max_deliver * ack_wait` (i.e.,
   `5 * 300s = 1500s`). A safe default is `3600s` (1 hour).

This is the designated post-PR2 follow-up documented in
`greentic_aw_runtime::serve::RuntimeAgentDispatchInvoker` (see the doc comment on
the `ledger` field).

---

## Verification Checklist

After deploying:

- [ ] `nats stream info greentic-agentic` shows expected configuration
- [ ] `nats consumer info greentic-agentic agentic-workers` shows 0 pending, 0 in-flight
- [ ] KEDA `ScaledObject` status shows `Active: False` (zero replicas while idle)
- [ ] Send a test request: `kubectl exec -it <runner-pod> -- curl -X POST ...`
      and verify `aw-serve` scales up (check `kubectl get pods -n greentic -w`)
- [ ] `aw-serve` pod logs show `"aw serve: warm targets configured"` (if `GREENTIC_AW_WARM_PACKS` is set)
- [ ] `aw-serve` pod logs show JetStream consumer connected and polling
- [ ] After request completes and `cooldownPeriod` elapses, replica count returns to 0
- [ ] `nats consumer info` shows 0 unprocessed messages
