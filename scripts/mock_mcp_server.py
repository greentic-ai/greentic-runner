#!/usr/bin/env python3
"""Minimal mock MCP HTTP server for the flow-node smoke test.

Speaks the streamable-HTTP MCP JSON-RPC contract the greentic runner's
``McpToolSource`` drives (see ``crates/greentic-aw-runtime/src/mcp_source.rs``
and the wiremock contract in ``crates/greentic-runner-host/tests/mcp_flow_node.rs``):

  POST /  { "method": "initialize", ... }              -> result + Mcp-Session-Id
  POST /  { "method": "notifications/initialized" }     -> 202 (no body)
  POST /  { "method": "tools/list", ... }               -> { tools: [...] }
  POST /  { "method": "tools/call", ... }               -> { structuredContent: {...} }

It exposes exactly ONE tool, ``get_issue``, and records every ``tools/call`` it
receives to a JSON sidecar file (``--call-log``) so the smoke can assert the
runner actually reached it.

No external deps (stdlib http.server only). Honest by design: this is a stand-in
for a real MCP server, NOT a real one.
"""

from __future__ import annotations

import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOL_NAME = "get_issue"

# Tool result returned for every tools/call. ``ToolOutput::to_value`` unwraps
# ``structuredContent``, so the runner binds exactly this object.
CALL_RESULT = {"structuredContent": {"title": "Bug", "source": "mock-mcp"}}

_CALL_LOG_PATH: str | None = None
_LOG_LOCK = threading.Lock()


def _record_call(body: dict) -> None:
    if _CALL_LOG_PATH is None:
        return
    entry = {
        "tool": body.get("params", {}).get("name"),
        "arguments": body.get("params", {}).get("arguments"),
    }
    with _LOG_LOCK:
        with open(_CALL_LOG_PATH, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")


class Handler(BaseHTTPRequestHandler):
    # Silence default request logging; the smoke owns the log output.
    def log_message(self, *_args) -> None:  # noqa: D401, ANN002
        return

    def _send_json(self, status: int, payload: dict | None) -> None:
        body = b"" if payload is None else json.dumps(payload).encode("utf-8")
        self.send_response(status)
        if payload is not None:
            self.send_header("Content-Type", "application/json")
        self.send_header("Mcp-Session-Id", "smoke-sess-1")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0") or "0")
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            self._send_json(400, {"error": "invalid json"})
            return

        method = body.get("method", "")
        req_id = body.get("id", 1)

        if method == "initialize":
            self._send_json(
                200,
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "serverInfo": {"name": "mock-mcp", "version": "1.0.0"},
                    },
                },
            )
            return

        if method == "notifications/initialized":
            self._send_json(202, None)
            return

        if method == "tools/list":
            self._send_json(
                200,
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "tools": [
                            {
                                "name": TOOL_NAME,
                                "description": "Get an issue (mock)",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"id": {"type": "string"}},
                                },
                            }
                        ]
                    },
                },
            )
            return

        if method == "tools/call":
            _record_call(body)
            self._send_json(
                200,
                {"jsonrpc": "2.0", "id": req_id, "result": CALL_RESULT},
            )
            return

        self._send_json(
            200,
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32601, "message": f"method not found: {method}"},
            },
        )


def main() -> int:
    global _CALL_LOG_PATH
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument(
        "--call-log",
        default=None,
        help="append each tools/call (tool + arguments) as JSONL to this file",
    )
    args = parser.parse_args()
    _CALL_LOG_PATH = args.call_log

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(
        f"mock-mcp listening on http://{args.host}:{args.port} "
        f"(tool={TOOL_NAME}, call_log={_CALL_LOG_PATH})",
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
