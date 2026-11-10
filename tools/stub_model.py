#!/usr/bin/env python3
"""Stub OpenAI-compatible chat endpoint for bench verification.

The board's agent loop must be provable without credentials, so this speaks just
enough of /v1/chat/completions: the first request answers with a `board_status`
tool call, and once the board sends the tool result back it answers with a final
text that quotes it. That exercises exactly the code path that matters - request
build, tool dispatch, tool message, second request - on real hardware.

    python tools/stub_model.py --port 8899
"""

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOL_NAME = "board_status"


def completion(message, finish):
    return {
        "id": "stub",
        "object": "chat.completion",
        "model": "stub",
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
    }


class Handler(BaseHTTPRequestHandler):
    def _send(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self._send({"ok": True, "service": "stub-model"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        messages = body.get("messages") or []
        tools = [t.get("function", {}).get("name") for t in (body.get("tools") or [])]
        tool_results = [m for m in messages if m.get("role") == "tool"]
        print(
            f"POST {self.path} model={body.get('model')} msgs={len(messages)} "
            f"tools={len(tools)} tool_results={len(tool_results)}",
            flush=True,
        )

        if not tool_results:
            if TOOL_NAME not in tools:
                self._send(
                    completion(
                        {
                            "role": "assistant",
                            "content": f"stub: {TOOL_NAME} is not in the tool list ({tools})",
                        },
                        "stop",
                    )
                )
                return
            self._send(
                completion(
                    {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [
                            {
                                "id": "call_stub_1",
                                "type": "function",
                                "function": {"name": TOOL_NAME, "arguments": "{}"},
                            }
                        ],
                    },
                    "tool_calls",
                )
            )
            return

        last = str(tool_results[-1].get("content", ""))
        self._send(
            completion(
                {
                    "role": "assistant",
                    "content": "stub heard back from the board: " + last[:200],
                },
                "stop",
            )
        )

    def log_message(self, *args):
        pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--host", default="0.0.0.0")
    args = ap.parse_args()
    print(f"stub model on http://{args.host}:{args.port}/v1/chat/completions", flush=True)
    ThreadingHTTPServer((args.host, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
