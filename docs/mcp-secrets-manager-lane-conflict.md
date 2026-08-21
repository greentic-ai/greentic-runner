# MCP credentials: `research` and `develop` disagree, on purpose, and one must lose

**Status:** open decision. Both sides are merged, on different lanes, and they
cancel each other out.
**Written:** 2026-08-21, after each side was verified separately.

Two PRs a day apart changed how an MCP tool's credential is resolved, and they
moved in **opposite directions**. Neither is wrong about the lane it was written
for. They cannot both survive a lane merge, and whichever is applied second will
silently undo the other — the failure it produces is a dispatch that looks like a
successful call and carries no credential, which is exactly what both PRs set
out to remove.

Do not reconcile them by taking whichever branch merges last.

## The two sides

| | `develop` — #705 | `research` — #706 |
|---|---|---|
| Path changed | flow node (`mcp_node`) | agent loop (`agent_node`) |
| Credential comes from | the manager the **host injected** (`PackRuntime`) | the manager built from **`SECRETS_BACKEND`** |
| `mcp_node::aw::secrets_from_env` | **deleted** | **relied on** |
| Written for | an operator bundle under `greentic-start` | a runner started directly against a broker |

## Why each is right where it was written

**#705.** `SecretsBackend::from_env` knows only `env` and `broker`. An operator
booting a bundle runs under `greentic-start`, whose own backend kinds are
`{DevStore, Env, Vault}` — the runner has no dev-store variant at all. So in
that lane, reading `SECRETS_BACKEND` cannot resolve a pack route's credential no
matter what it is set to: unset, the `env` backend cannot parse a `secrets://`
URI; set to `dev-store`, `from_env` errors and the manager is `None`. The host's
injected manager is the only thing there that can read the token.

**#706.** greentic-designer's `orchestrate::mcp_runtime` projects
`SECRETS_BACKEND=broker` plus `SECRETS_BROKER_*` onto the children it spawns,
and the agent loop never read them — so an agentic worker's `mcp:` tool
resolved against whatever manager the host injected and failed with "no
credential", having contacted no broker at all. Verified end to end against a
real broker and a real MCP server: before, zero broker requests; after, the
team-scoped URI is fetched and the tool dispatches with the token.

## Why this is not simply "pick the injected manager"

That is the obvious reconciliation and it is not obviously right. The injected
manager is correct only when the host actually supplies one that can resolve the
URI. Three hosts, three answers:

- `greentic-start` — injects a dev store that *can* hold the token. #705's case.
- a bare `greentic-runner` given `SECRETS_BACKEND=broker` — its injected manager
  comes from `SecretsBackend::from_config(config.secrets)`, whose `kind` is read
  from a **config file** and never from the environment. So an operator who
  exports the variable and no config gets the `env` backend injected, and the
  broker is unreachable. #706's case, and the reason it was needed.
- greentic-designer's in-process host — injects its own, and neither PR affects
  it.

So "always injected" breaks the second, and "always env" breaks the first. A
correct rule has to distinguish them, and the cheapest honest one is probably:
**prefer the injected manager, and let `SECRETS_BACKEND` override only when it
is explicitly set** — which is what #706's `choose_mcp_secrets` already does for
the agent path, with an unset variable keeping the injected manager. Applying
that same rule to the flow path would let #705 keep what it needed without
deleting the env route. That is a proposal, not a decision.

## What is blocked behind this

`greentic-start` cannot receive either fix yet, for a reason that has nothing to
do with which one wins: it pins

    greentic-runner-host = ">=1.2.0-dev.29565371086, <1.3.0-0"

and the `research` lane publishes `1.3.0-research.N`. #706 is therefore
unreachable from that lane by construction; only a `1.2.x-dev` publish carrying
the fix can reach it. #705 is on `develop`, which does publish that line.

#705 also records a further blocker in its own body, unrelated to this conflict:
greentic-start's dev-store client canonicalizes the key segment of every
`secrets://` URI, rewriting an MCP server's hyphenated UUID into something that
resolves nothing.

## Consequence for anyone reading a status report

An agentic worker's `mcp:` tool is **proven to work** against a directly-run
`greentic-runner` with a broker, and is **not** demonstrated in the
`operator_env` lane. The designer-side spec
(`greentic-designer/docs/superpowers/specs/2026-08-20-aw-mcp-in-deployed-bundles-design.md`)
marks that lane NOT exercised for this reason. Do not upgrade that row on the
strength of #706 alone.
