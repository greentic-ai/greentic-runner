# Enabling extension tools for a Digital Worker (agentic worker)

An agentic-worker agent (`DwAgent` flow node) calls extension tools through the
`greentic-ext-runtime`. Each tool the agent may use is declared as a
`ToolRef { extension_id, tool_name }` in the agent's `AgentConfig`.

## Method 1 — declare tools in operator YAML (always available)

The runner-host config (`HostConfig`) carries an `agents:` map. Each entry is a
full `AgentConfig`: the system prompt, the LLM provider/model, limits, and the
tool list.

```yaml
agents:
  research-bot:
    agent_id: research-bot
    system_prompt: "You are a research assistant. Use tools when helpful."
    llm:
      provider: openai
      model: gpt-4o-mini
    tools:
      - extension_id: greentic.tavily
        tool_name: web_search
      - extension_id: greentic.sql
        tool_name: sql_ask
    limits:
      max_iter: 8
      timeout: 60
      max_history_turns: 20
      llm_retry_attempts: 3
      llm_retry_backoff: 250
```

At runtime the loop resolves each `ToolRef` against the loaded extension via
`ExtensionRuntime::list_tools` — a tool whose extension is not installed is
logged and silently skipped (the LLM simply never sees it).

Prerequisites:
- The extension is installed in the extension discovery dir
  (`GREENTIC_EXTENSIONS_DIR`, else `~/.greentic/extensions`).
- `GREENTIC_AW_REDIS_URL` is set (the agent loop persists session state in Redis).

## Method 2 — manifest overlay

Instead of hand-listing `tools:` in YAML, drop the Digital Worker's manifest
JSON into the manifests dir and the runner overlays its `agentic_worker`-capable
tools onto the agent's `tools` list automatically.

- **Where:** `GREENTIC_AGENT_MANIFESTS_DIR` (else `~/.greentic/agents/`), file
  named `<agent_id>.json`.
- **What it is:** the `DigitalWorkerManifest` JSON the DW compose wizard emits
  (`greentic-dw` wizard stdout — capture it to a file).
- **What it overrides:** only `AgentConfig.tools`. The operator YAML `agents:`
  entry still supplies `system_prompt`, `llm`, and `limits` — those are NOT in
  the manifest.
- **Fail-soft:** a missing, malformed, invalid, or id-mismatched manifest is
  logged and ignored; the agent falls back to the YAML `tools:` list. A valid
  manifest that declares *no* agentic-worker tools is treated as "no tool
  opinion" and likewise leaves the YAML list intact (it never silently wipes it).
  A broken manifest never takes the agent offline.

Example: with `~/.greentic/agents/research-bot.json` present, the YAML
`agents.research-bot` needs only `system_prompt` + `llm` (+ optional `limits`);
its `tools:` may be empty and will be replaced by the manifest's tool set at
load time.

> The DW wizard writes this file for you: `gtc wizard … --emit-manifest <DIR>`
> emits `<DIR>/<manifest_id>.json` — the exact loose file this overlay reads.
> (The manifest is not stored inside the composed `.gtpack`, so this loose file
> is the supported delivery path.)
