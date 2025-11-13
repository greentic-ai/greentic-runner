# greentic-runner

Monorepo for the official Greentic runner host (binary + library) and integration tests.  
`greentic-runner` (binary) wraps `crates/greentic-runner-host`, the production runtime for canonical adapters, session/state glue, and admin plumbing; any legacy `greentic-host` references are deprecated and should be replaced with this stack. Timer/cron flows are intentionally not supported here—the plan is to handle scheduled/event sources via a future `greentic-events` project while the runner focuses on sessionful messaging and webhooks.

## Quick start

```bash
# Configure env vars (see host README for the full table)
export PACK_INDEX_URL=./examples/index.json
export PACK_CACHE_DIR=.packs
export DEFAULT_TENANT=demo

# Run the HTTP host on port 8080
cargo run -p greentic-runner -- --bindings examples/bindings/demo.yaml --port 8080

# Trigger a Telegram-style webhook
curl -X POST http://localhost:8080/messaging/telegram/webhook \
  -H "Content-Type: application/json" \
  -d '{"update_id":1,"message":{"chat":{"id":42},"text":"hello"}}'
```

The host loads packs declared in `PACK_INDEX_URL`, verifies signatures/digests (via `PACK_PUBLIC_KEY` / `PACK_VERIFY_STRICT`), and exposes the built-in adapters. Every ingress payload (Telegram/WebChat/Slack/Webex/WhatsApp/webhook) is normalized into the canonical schema with deterministic session keys so pause/resume + dedupe work the same way across providers.

## Pack index schema

Pack resolution is driven by a JSON index (see `examples/index.json`). Each tenant entry supplies a `main_pack` plus optional ordered `overlays`:

```json
{
  "tenants": {
    "demo": {
      "main_pack": {
        "reference": { "name": "demo-pack", "version": "1.2.3" },
        "locator": "fs:///packs/demo.gtpack",
        "digest": "sha256:abcd...",
        "signature": "ed25519:...",
        "path": "./packs/demo.gtpack"
      },
      "overlays": [
        {
          "reference": { "name": "demo-overlay", "version": "1.2.3" },
          "path": "./packs/demo-overlay.gtpack",
          "digest": "sha256:efgh..."
        }
      ]
    }
  }
}
```

During a reload the watcher resolves each locator (filesystem, HTTPS, OCI, S3, GCS, or Azure blob), validates the digest/signature, populates the content-addressed cache, warms Wasmtime, and swaps the `TenantRuntime` atomically. Overlays can be added/removed tenant-by-tenant without touching the base pack; `crates/tests/tests/host_integration.rs` contains a regression test for overlay reloads.

## Sessions & pause/resume

Packs can emit the `session.wait` component to pause execution (e.g., waiting for a human reply). `greentic-runner-host` automatically:

1. Serializes the `FlowSnapshot` (next node + execution state) into `greentic-session`.
2. Uses a canonical session key (`tenant:provider:channel:conversation:user`) hashed into a `UserId`, so the next inbound activity finds the correct snapshot.
3. Resumes the snapshot on the next activity, continues execution, and clears the stored state once the flow finishes.

No glue code is required inside packs; authors just emit `session.wait` and persist any additional state via `greentic-state`. The canonical session key format is `{tenant}:{provider}:{conversation-or-channel}:{user}` so every adapter participates consistently (documented in `crates/greentic-runner-host/README.md`).

## Repository layout

| Path | Description |
| --- | --- |
| `crates/greentic-runner-host/` | Production runtime crate (docs, canonical adapters, env table, admin API) |
| `crates/greentic-runner/` | Binary that embeds the host (CLI entrypoint) |
| `crates/tests/` | Integration test harness (demo pack execution, watcher reload/overlay regression, adapter fixtures) |
| `examples/` | Sample bindings, reference `index.json`, example packs |

## Development

```bash
cargo fmt
cargo clippy
cargo test
```

Integration tests under `crates/tests/tests/*.rs` exercise the demo pack, watcher reloads (including overlays), and scaffold future adapters (webhook). Enable new fixtures as adapters mature.

## Ingress adapters at a glance

| Provider | Route | Env/deps | Notes |
| --- | --- | --- | --- |
| Telegram Bot API | `POST /messaging/telegram/webhook` | `TELEGRAM_BOT_TOKEN` (used by the egress bridge) | Canonicalises update ids, dedupes via cache |
| Microsoft Teams (Bot Framework) | `POST /teams/activities` | None (HTTPS listener; add auth proxy externally) | Uses `replyToId`/conversation/channel to derive session key |
| Slack Events API | `POST /slack/events` | `SLACK_SIGNING_SECRET` | Handles `url_verification`, dedupes via `event_id` |
| Slack Interactivity | `POST /slack/interactive` | `SLACK_SIGNING_SECRET` | Parses `payload=` form body; same canonical contract |
| WebChat / Direct Line | `POST /webchat/activities` | None | Mirrors Bot Framework schema; attachments mapped 1:1 |
| Cisco Webex | `POST /webex/webhook` | `WEBEX_WEBHOOK_SECRET` (optional signature) | File URLs surfaced in canonical attachments |
| WhatsApp Cloud API | `GET/POST /whatsapp/webhook` | `WHATSAPP_VERIFY_TOKEN`, `WHATSAPP_APP_SECRET` | Normalizes interactive/list replies into canonical buttons |
| Generic Webhook | `ANY /webhook/:flow_id` | Idempotency via `Idempotency-Key` header | Passes normalized HTTP request object to the target flow |
All adapters emit the canonical payload (`tenant`, `provider`, `provider_ids`, `session.key`, `text`, `attachments`, `buttons`, `entities`, `metadata`, `channel_data`, `raw`). The canonical session key `{tenant}:{provider}:{conversation-or-thread-or-channel}:{user}` drives dedupe and pause/resume semantics universally.

## Environment variables

Common settings (full table lives in `crates/greentic-runner-host/README.md`):

- `PACK_INDEX_URL`, `PACK_CACHE_DIR`, `PACK_SOURCE` – control pack discovery & caching.
- `PACK_REFRESH_INTERVAL` – watcher cadence (e.g., `30s`, `5m`).
- `TENANT_RESOLVER`, `DEFAULT_TENANT` – HTTP routing behaviour (host/header/jwt/env).
- `SECRETS_BACKEND`, `OTEL_*` – bootstrap secrets + telemetry.
- `ADMIN_TOKEN` – protect `/admin/*` endpoints; loopback-only access when unset.

## Publishing

Versions are tracked per crate. Tagging `master` with `<crate>-vX.Y.Z` triggers the publish workflow which pushes the crate to crates.io. Use `ci/local_check.sh` before tagging to mirror the CI pipeline locally.

## Bindings inference

`greentic-gen-bindings` can inspect a pack directory and emit a complete `bindings.yaml` seed using the same schema the host expects:

```bash
cargo run -p greentic-runner --bin greentic-gen-bindings \
  --pack examples/weather-demo \
  --out generated/bindings.complete.yaml \
  --complete
```

`--complete` fills safe defaults for env passthrough, network allowlists, secrets, and MCP server stubs; `--strict` additionally fails if HTTP/secrets/MCP requirements cannot be satisfied so pack authors can share hints via `bindings.hints.yaml` or `meta.bindings` annotations. The CLI also understands `--component` so future packs compiled to a Wasm component can be inspected for host imports before generating bindings.

These emitted hints follow the canonical [`greentic-types::bindings::hints`](https://docs.rs/greentic-types/latest/greentic_types/bindings/hints/index.html) schema (network allowlists, env passthrough, secrets.required, and MCP servers), which keeps the host and generator speaking the same language.

## Future work

- Scheduled/timer-based flows will move to a dedicated `greentic-events` experience (similar to `greentic-messaging` for sessionful adapters). Until that project matures, the runner intentionally keeps those adapters out of the official host surface.

## License

MIT
