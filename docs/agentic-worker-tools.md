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

## Method 2 — auto-overlay tools from a Digital Worker manifest

See `docs/agentic-worker-tools.md#manifest-overlay` (added in Task 6) once the
manifest overlay ships. In short: drop the DW's `<agent_id>.json` manifest into
the manifests dir and its `agentic_worker`-capable tools replace the YAML
`tools:` list automatically; the YAML still supplies `system_prompt` + `llm`.
