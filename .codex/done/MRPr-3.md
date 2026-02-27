PR-03 — greentic-runner: Pack-aware multi-pack execution + interruptible sessions using ReplyScope

Repo: greenticai/greentic-runner
Goal: True multi-tenant + multi-pack + interruptible sessions, without runner knowing channels/providers.

Part 1: Make gtbind runtime-authoritative (but pack-only)
A) Loader: --bindings <file|dir>

Load one or many *.gtbind

Merge into HostConfig per tenant

Allow both:

--bindings file.gtbind

--bindings-dir ./bindings/

B) gtbind required fields (NO channels section)
tenant: dev.local
pack_id: sha256:...
pack_ref: store://hello-pack@1.0.0

flows:
  - id: main

env_passthrough:
  - OTEL_EXPORTER_OTLP_ENDPOINT


Rules:

runner rejects gtbind without pack_id

flows are registered as belonging to that pack_id

env_passthrough is an allowlist merged with runner policy

Part 2: Pack-aware flow registry and state

Flow registry key becomes (tenant_id, pack_id, flow_id)

State prefix passed to greentic-state includes pack_id:

pack/{pack_id}/flow/{flow_id}/user/{user_id} (or session id)

Part 3: Interruptible sessions (WAIT/RESUME) via greentic-session multi-wait
On session.wait

Extract reply_scope from the current inbound context (the inbound message/activity must carry it)

If missing, error with a clear message:

“Cannot suspend: reply_scope missing; provider plugin must supply ReplyScope”

Store SessionData { pack_id: Some(pack_id), ... }

Call greentic_session::register_wait(...)

Return suspended outcome

On inbound message

Require inbound activity has reply_scope

Use find_wait_by_scope(ctx, user_id, reply_scope)

Load SessionData blob, resume snapshot cursor

On completion:

clear_wait(...)

Correlation id handling

When runner sends a message that expects a reply, generate a correlation id and attach it to outbound metadata.

Provider/plugin should echo it back into inbound reply_scope.correlation if possible.

Tests (mandatory)

Multi-pack: same tenant, two packs, both have flow_id=main → both load → both runnable

Multi-wait: same user triggers two waits with different reply_scope (e.g. different conversation/thread) → replies resume correct one

LB safety: simulate restart (or second runner instance) using Redis session/state backends → resume still works
