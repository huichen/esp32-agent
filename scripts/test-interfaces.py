#!/usr/bin/env python3
"""Live HTTP/MCP/agent regressions; Python 3 stdlib, loopback fixtures only.

Build: cargo build -p esp32-agent-sim
Run:   python scripts/test-interfaces.py --report target/interfaces.json
Focus: python scripts/test-interfaces.py --areas rpc,permissions,transport

Each area owns a --port 0 simulator and a TemporaryDirectory. No sim-state,
providers, credentials, hardware, or processes belonging to the operator are used.
Assertions are not xfailed: a confirmed regression makes the process exit 1.
The JSON report records every assertion, exercised routes/methods/tools, and gaps.
Schemas/limits come from agent.json, tools/list, files/tree and the owning Rust
modules. Protocol edge expectations follow JSON-RPC 2.0 batch/id rules; tolerant
legacy aliases are intentionally exercised, not mistaken for advertised tools.
"""

import argparse
import base64
import collections
import dataclasses
import fnmatch
import hashlib
import http.client
import http.server
import json
import math
import os
from pathlib import Path
import re
import secrets
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.parse


ROOT = Path(__file__).resolve().parents[1]
MISSING = object()
MARK = "…(truncated)"
MARK_BYTES = len(MARK.encode("utf-8"))
RUN_DEADLINE = None


class DeadlineExpired(TimeoutError):
    pass


def bounded_timeout(seconds):
    """All socket/poll operations share the run budget; cleanup gets up to 8 s."""
    if RUN_DEADLINE is None:
        return seconds
    remaining = RUN_DEADLINE - time.monotonic()
    if remaining <= 0:
        raise DeadlineExpired("overall run budget exceeded")
    return min(seconds, remaining)


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode("utf-8")


def short(value, limit=1600):
    text = json.dumps(value, ensure_ascii=False, default=str)
    return text if len(text) <= limit else text[:limit] + "… [report excerpt]"


def wait_for(get, predicate, seconds=8):
    deadline = time.monotonic() + bounded_timeout(seconds)
    last = None
    while time.monotonic() < deadline:
        last = get()
        if predicate(last):
            return last
        time.sleep(0.04)
    bounded_timeout(1)  # Distinguish an overall deadline from this condition's timeout.
    raise TimeoutError("condition did not become true: " + short(last))


@dataclasses.dataclass
class Reply:
    status: int
    headers: dict
    body: bytes

    def json(self):
        return json.loads(self.body.decode("utf-8")) if self.body else None


class Recorder:
    def __init__(self, verbose):
        self.verbose = verbose
        self.area = "setup"
        self.checks = []
        self.case_names = set()
        self.routes = collections.defaultdict(set)
        self.methods = collections.defaultdict(set)
        self.tools = collections.defaultdict(set)
        self.refusals = collections.defaultdict(set)
        self.commands = collections.defaultdict(set)
        self.notes = collections.defaultdict(list)
        self.successful_tools = collections.defaultdict(set)
        self.model_requested_tools = collections.defaultdict(set)
        self.model_error_tools = collections.defaultdict(set)
        self.model_successful_tools = collections.defaultdict(set)
        self.inventory = {}
        self.area_summary = {}

    def check(self, name, condition, detail=""):
        key = self.area + "/" + name
        if key in self.case_names:
            raise ValueError("duplicate assertion: " + key)
        self.case_names.add(key)
        result = {"case": key, "passed": bool(condition), "detail": str(detail)[:2000]}
        self.checks.append(result)
        if not condition or self.verbose:
            print(("PASS " if condition else "FAIL ") + key + (" : " + result["detail"] if not condition else ""), flush=True)
        return bool(condition)

    def equal(self, name, actual, expected):
        return self.check(name, actual == expected, "expected " + short(expected) + "; got " + short(actual))

    def note(self, text):
        self.notes[self.area].append(text)

    def coverage(self):
        areas = sorted(set(c["case"].split("/")[0] for c in self.checks))
        return {a: {"routes": sorted(self.routes[a]), "rpc_methods": sorted(self.methods[a]),
                    "called_tools": sorted(self.tools[a]), "refused_tools": sorted(self.refusals[a]),
                    "successful_tools": sorted(self.successful_tools[a]),
                    "model_requested_tools": sorted(self.model_requested_tools[a]),
                    "model_successful_tools": sorted(self.model_successful_tools[a]),
                    "model_error_tools": sorted(self.model_error_tools[a]),
                    "family_commands": sorted(self.commands[a]), "notes": self.notes[a]} for a in areas}


class Simulator:
    def __init__(self, binary, directory, rec, initial=None, empty=False):
        self.binary, self.directory, self.rec = binary, Path(directory), rec
        self.directory.mkdir(parents=True, exist_ok=True)
        self.state = self.directory / "kv.json"
        if initial is not None:
            self.state.write_bytes(encode(initial))
        self.empty = empty
        self.process = self.log = None
        self.port, self.sid, self.serial = None, None, 0

    def __enter__(self):
        self.start()
        return self

    def __exit__(self, *_):
        self.stop()

    def start(self):
        self.log = open(self.directory / "sim.log", "wb")
        command = [str(self.binary), "--host", "127.0.0.1", "--port", "0", "--state", str(self.state)]
        if self.empty:
            command.append("--empty")
        self.process = subprocess.Popen(command, cwd=ROOT, stdout=self.log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + bounded_timeout(10)
        while time.monotonic() < deadline:
            text = (self.directory / "sim.log").read_text(encoding="utf-8", errors="replace")
            found = re.search(r"portal\s+http://127\.0\.0\.1:(\d+)", text)
            if found:
                self.port = int(found[1])
                break
            if self.process.poll() is not None:
                self.stop()
                raise RuntimeError("simulator exited during startup: " + text[-2000:])
            time.sleep(0.04)
        else:
            self.stop()
            raise TimeoutError("simulator did not announce its ephemeral port")
        self.sid = None
        self.get("/healthz")
        self.post("/api/agent/config", {"tick_ms": 200})

    def stop(self):
        # Only the exact Popen child we created, never a port/process-name search.
        if self.process is not None:
            if self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=3)
            self.process = None
        if self.log is not None:
            self.log.close()
            self.log = None

    def restart(self):
        self.drain()
        self.stop()
        self.start()

    @property
    def url(self):
        return "http://127.0.0.1:" + str(self.port)

    def request(self, method, path, payload=MISSING, headers=None, timeout=4, chunked=False):
        self.rec.routes[self.rec.area].add(method + " " + path.split("?", 1)[0])
        for message in payload if isinstance(payload, list) else [payload]:
            if isinstance(message, dict) and isinstance(message.get("method"), str):
                self.rec.methods[self.rec.area].add(message["method"])
                if message["method"] == "tools/call" and isinstance(message.get("params"), dict):
                    name = message["params"].get("name")
                    if isinstance(name, str):
                        self.rec.tools[self.rec.area].add(name)
        body = None if payload is MISSING else payload if isinstance(payload, bytes) else encode(payload)
        head = {"Connection": "close", "Content-Type": "application/json"}
        head.update(headers or {})
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=bounded_timeout(timeout))
        try:
            if chunked:
                head["Transfer-Encoding"] = "chunked"
                body = [body[i:i + 7] for i in range(0, len(body), 7)]
            conn.request(method, path, body=body, headers=head, encode_chunked=chunked)
            response = conn.getresponse()
            data = response.read(5 * 1024 * 1024 + 1)
            if len(data) > 5 * 1024 * 1024:
                raise ValueError("test client response exceeded its 5 MiB ceiling")
            return Reply(response.status, {k.lower(): v for k, v in response.getheaders()}, data)
        finally:
            conn.close()

    def get(self, path, headers=None):
        return self.request("GET", path, headers=headers).json()

    def post(self, path, payload, headers=None):
        return self.request("POST", path, payload, headers).json()

    def rpc(self, method, params=None, ident=MISSING, headers=None):
        self.serial += 1
        self.rec.methods[self.rec.area].add(method)
        message = {"jsonrpc": "2.0", "id": self.serial if ident is MISSING else ident, "method": method}
        if params is not None:
            message["params"] = params
        head = {"Mcp-Session-Id": self.sid} if self.sid else {}
        head.update(headers or {})
        response = self.request("POST", "/mcp", message, head)
        self.sid = response.headers.get("mcp-session-id", self.sid)
        return response

    def call(self, name, arguments=None):
        self.rec.tools[self.rec.area].add(name)
        if name in {"board", "bus", "manifest", "display", "indicators", "audio"} and isinstance(arguments, dict):
            self.rec.commands[self.rec.area].add(name + " " + str(arguments.get("command")))
        reply = self.rpc("tools/call", {"name": name, "arguments": {} if arguments is None else arguments}).json()
        result = reply.get("result", {})
        if result.get("isError"):
            self.rec.refusals[self.rec.area].add(name)
        elif result.get("isError") is False:
            self.rec.successful_tools[self.rec.area].add(name)
        return result

    def names(self):
        return [t["name"] for t in self.rpc("tools/list").json()["result"]["tools"]]

    def drain(self):
        return wait_for(lambda: self.get("/api/status")["store"], lambda s: s["pending"] == 0, 8)

    def raw(self, wire, finish=True):
        with socket.create_connection(("127.0.0.1", self.port), timeout=bounded_timeout(4)) as conn:
            conn.settimeout(bounded_timeout(4))
            conn.sendall(wire)
            if finish:
                conn.shutdown(socket.SHUT_WR)
            response = http.client.HTTPResponse(conn)
            response.begin()
            return Reply(response.status, {k.lower(): v for k, v in response.getheaders()}, response.read())


def content(result):
    if "structuredContent" in result:
        return result["structuredContent"]
    blocks = result.get("content", [])
    text = blocks[0].get("text", "") if blocks else ""
    try:
        return json.loads(text)
    except (ValueError, TypeError):
        return {"text": text}


def success(rec, sim, case, name, args=None):
    result = sim.call(name, args)
    blocks = result.get("content", [])
    rec.check(case + ".result-shape", result.get("isError") is False and bool(blocks)
              and all(b.get("type") == "text" and isinstance(b.get("text"), str) for b in blocks), short(result))
    value = content(result)
    if "structuredContent" in result:
        rec.equal(case + ".structured-text-match", json.loads(blocks[0]["text"]), value)
    return value


def refusal(rec, sim, case, name, args=None, contains=None):
    result = sim.call(name, args)
    text = " ".join(b.get("text", "") for b in result.get("content", []))
    rec.check(case, result.get("isError") is True and bool(text)
              and (contains is None or contains in text), short(result))
    return result


def http_case(rec, sim, case, method, path, payload=MISSING, status=200, headers=None):
    response = sim.request(method, path, payload, headers)
    rec.equal(case + ".status", response.status, status)
    return response.json() if response.body and response.headers.get("content-type", "").startswith("application/json") else None


class FixtureServer:
    """Bounded raw replies behind a normal stdlib request parser; no outbound I/O."""
    def __init__(self):
        self.lock = threading.Lock()
        self.plans = {}
        self.seen = collections.defaultdict(list)
        self.cancel = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def setup(self):
                super().setup()
                self.connection.settimeout(4)

            def log_message(self, *_):
                pass

            def do_GET(self):
                self.answer()

            def do_POST(self):
                self.answer()

            def answer(self):
                self.connection.settimeout(4)
                length = int(self.headers.get("Content-Length", "0"))
                if not 0 <= length <= 4 * 1024 * 1024:
                    self.send_error(413)
                    return
                raw = self.rfile.read(length)
                try:
                    body = json.loads(raw) if raw else None
                except ValueError:
                    body = raw.decode("utf-8", "replace")
                path = urllib.parse.urlsplit(self.path).path
                with owner.lock:
                    seen = owner.seen[path]
                    index = len(seen)
                    if index >= 80:
                        self.send_error(429)
                        return
                    seen.append({"method": self.command, "target": self.path,
                                 "headers": {k.lower(): v for k, v in self.headers.items()}, "body": body})
                    plan = owner.plans.get(path, {})
                    replies = plan.get("replies", [{"error": "unknown test fixture"}])
                    answer = replies[min(index, len(replies) - 1)]
                if owner.cancel.wait(plan.get("delay", 0)):
                    return
                statuses = plan.get("statuses", [plan.get("status", 200)])
                status = statuses[min(index, len(statuses) - 1)]
                dialect = plan.get("dialect")
                if dialect and not valid_model_request(dialect, body):
                    status, answer = 400, {"error": "fixture expected " + dialect + " request shape"}
                if plan.get("echo"):
                    answer = {"method": self.command, "target": self.path, "body": body}
                payload = answer if isinstance(answer, bytes) else encode(answer)
                mode = plan.get("mode", "fixed")
                prefix = b"HTTP/1.1 100 Continue\r\n\r\n" if mode == "interim" else b""
                head = "HTTP/1.1 %d Fixture\r\nContent-Type: application/json\r\nConnection: close\r\n" % status
                if mode == "chunked":
                    wire = b""
                    for chunk in (payload[:7], payload[7:19], payload[19:]):
                        if chunk:
                            wire += ("%x;fixture=yes\r\n" % len(chunk)).encode() + chunk + b"\r\n"
                    wire += b"0\r\nX-Fixture: complete\r\n\r\n"
                    head += "Transfer-Encoding: chunked\r\n"
                elif mode == "bad-chunk":
                    head += "Transfer-Encoding: chunked\r\n"
                    wire = b"not-hex\r\n" + payload + b"\r\n0\r\n\r\n"
                elif mode == "duplicate-length":
                    head += "Content-Length: %d\r\ncontent-length: %d\r\n" % (len(payload), len(payload))
                    wire = payload
                elif mode == "close":
                    wire = payload
                else:
                    length = 65537 if mode == "oversized" else len(payload) + (7 if mode == "short" else 0)
                    head += "Content-Length: %d\r\n" % length
                    wire = b"" if mode == "oversized" else payload
                try:
                    self.connection.sendall(prefix + head.encode("ascii") + b"\r\n" + wire)
                except (OSError, TimeoutError):
                    pass  # A client testing timeout/limit refusal deliberately disconnects.
                self.close_connection = True

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=0.05), daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.cancel.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def fixture(self, path, replies, **options):
        with self.lock:
            self.plans[path] = {"replies": replies, **options}
            self.seen[path] = []
        return "http://127.0.0.1:%d%s" % (self.server.server_port, path)

    def requests(self, path):
        with self.lock:
            return list(self.seen[path])


def valid_model_request(protocol, body):
    if not isinstance(body, dict):
        return False
    if protocol == "responses":
        return isinstance(body.get("input"), list) and "messages" not in body
    if protocol == "anthropic":
        # Messages requires max_tokens, not system: a direct image_read has no
        # system turn. Still reject Chat/Responses image parts rather than
        # making a wrongly selected image adapter look successful.
        messages = body.get("messages")
        return (isinstance(messages, list) and bool(messages)
                and ("system" not in body or isinstance(body["system"], (str, list)))
                and type(body.get("max_tokens")) is int and body["max_tokens"] > 0
                and all(isinstance(m, dict) and m.get("role") in {"user", "assistant"}
                        and not any(isinstance(p, dict) and p.get("type") in {"image_url", "input_image", "input_text"}
                                    for p in (m.get("content", []) if isinstance(m.get("content"), list) else []))
                        for m in messages))
    return isinstance(body.get("messages"), list) and "input" not in body


def model_reply(protocol="chat", text="fixture final ✓", calls=(), reasoning="fixture reasoning", usage=(11, 7)):
    # A call tuple is (id, name, arguments); raw malformed strings stay malformed.
    if protocol == "chat":
        message = {"role": "assistant", "content": text, "reasoning_content": reasoning}
        if calls:
            message["tool_calls"] = [{"id": ident, "type": "function", "function": {
                "name": name, "arguments": args if isinstance(args, str) else encode(args).decode()}}
                for ident, name, args in calls]
        return {"choices": [{"message": message, "finish_reason": "tool_calls" if calls else "stop"}],
                "usage": {"prompt_tokens": usage[0], "completion_tokens": usage[1]}}
    if protocol == "responses":
        output = [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}] if text else []
        output += [{"type": "function_call", "call_id": ident, "name": name,
                    "arguments": args if isinstance(args, str) else encode(args).decode()} for ident, name, args in calls]
        return {"output": output, "usage": {"input_tokens": usage[0], "output_tokens": usage[1]}}
    blocks = [{"type": "thinking", "thinking": reasoning}] if reasoning else []
    if text:
        blocks.append({"type": "text", "text": text})
    blocks += [{"type": "tool_use", "id": ident, "name": name, "input": args} for ident, name, args in calls]
    return {"content": blocks, "usage": {"input_tokens": usage[0], "output_tokens": usage[1]}}


def run_turn(sim, text="audit question", attachments=None, seconds=8):
    after = sim.get("/api/agent/events")["seq"]
    body = {"text": text}
    if attachments is not None:
        body["attachments"] = attachments
    queued = sim.request("POST", "/api/agent/message", body)
    if queued.status != 202:
        raise RuntimeError("turn was not queued: " + short(queued.json()))
    return finish_turn(sim, after, seconds)


def finish_turn(sim, after, seconds=8, runs=1):
    events = wait_for(lambda: sim.get("/api/agent/events?after=" + str(after)),
                      lambda v: sum(e["event"] == "run_end" for e in v["events"]) >= runs, seconds)
    history = sim.get("/api/agent/history")
    names = {c["id"]: c["name"] for m in history["messages"] for c in m.get("tool_calls", [])}
    sim.rec.model_requested_tools[sim.rec.area].update(names.values())
    for message in history["messages"]:
        if message["role"] == "tool" and message.get("tool_call_id") in names:
            target = sim.rec.model_error_tools if message.get("is_error") else sim.rec.model_successful_tools
            target[sim.rec.area].add(names[message["tool_call_id"]])
    return history, events["events"]


def paired(messages):
    waiting = []
    for message in messages:
        if message.get("pending"):
            continue
        if message["role"] == "assistant":
            if waiting:
                return False
            waiting = [t["id"] for t in message.get("tool_calls", [])]
        elif message["role"] == "tool":
            ident = message.get("tool_call_id")
            if ident not in waiting:
                return False
            waiting.remove(ident)
        elif waiting:
            return False
    return not waiting


def discovery(rec, sim, fake):
    doc = sim.get("/agent.json")
    endpoints = doc["endpoints"]
    inventory = {(e["method"], e["path"]) for e in endpoints}
    rec.equal("inventory-count", len(endpoints), doc["counts"]["endpoints"])
    rec.equal("inventory-unique", len(inventory), len(endpoints))
    rec.equal("discovered-base-url", doc["dynamic"]["base_url"], sim.url)
    rec.equal("sitemap-alias", sim.get("/sitemap.json")["endpoints"], endpoints)
    rec.check("inventory-descriptions", all(e.get("tag") and e.get("summary") and e.get("returns") and "auth" in e for e in endpoints))
    for name, expected in [("tool_result_bytes", 65536), ("reply_bytes", 32768),
                           ("history_bytes", 2097152), ("history_messages", 1024),
                           ("saved_transcript_bytes", 524288), ("workspace_file_bytes", 262144),
                           ("workspace_text_bytes", 4194304)]:
        rec.equal("discovered-limit:" + name, doc["conventions"]["limits"].get(name), expected)
    tree = sim.get("/api/files/tree")
    specs = sim.rpc("tools/list", {}).json()["result"]["tools"]
    write_description = next(t["description"] for t in specs if t["name"] == "write")
    rec.check("write-description-shares-file-budgets", all(str(tree[k]) + " bytes" in write_description for k in ["file_max", "total_max"]), write_description)
    job = sim.post("/api/script", {"source": "1 + 2"})
    sim.call("write", {"path": "/map-fixture.txt", "text": "discovery fixture"})
    sim.post("/api/agent/message", {"text": "attachment discovery fixture", "attachments": [
        {"name": "map.bin", "mime": "application/octet-stream", "data_b64": "Zml4dHVyZQ=="}]})
    concrete = {"/api/script/<id>": "/api/script/" + job["id"],
                "/api/attachment/<name>": "/api/attachment/map.bin",
                "/api/files/read": "/api/files/read?path=/map-fixture.txt"}
    skips = {"/api/agent/stream", "/selftest/chat/completions"}
    for e in endpoints:
        if e["method"] != "GET" or e["path"] in skips:
            continue
        reply = sim.request("GET", concrete.get(e["path"], e["path"]))
        rec.equal("advertised-GET:" + e["path"], reply.status, 200)
        rec.check("body:" + e["path"], bool(reply.body) and bool(reply.headers.get("content-type")))
    # Contract regressions: the old map advertised GET selftest and omitted live routes.
    rec.check("selftest-advertises-POST", ("POST", "/selftest/chat/completions") in inventory, short(sorted(inventory)))
    for method, path in [("POST", "/api/tools"), ("GET", "/api/files"), ("GET", "/api/files/tree"),
                         ("GET", "/api/files/read"), ("POST", "/api/sched/run")]:
        rec.check("implemented-route-in-map:" + path, (method, path) in inventory,
                  "implemented in app.rs but absent from /agent.json")
    posts = {
        "/api/agent/prompt": {"action": "set", "text": "audit standing order"},
        "/api/config": {"device_name": "interface-audit"},
        "/api/manifest": {"manifest": {"version": 1, "tools": []}},
        "/api/tools": {"disabled": []},
        "/api/devices/probe": {}, "/api/wifi/connect": {"ssid": "fixture-network", "password": ""},
        "/api/wifi/forget": {}, "/api/agent/message": {"text": "no provider is configured"},
        "/api/agent/reset": {}, "/api/agent/forget": {"keep": 0}, "/api/agent/config": {"model": ""},
        "/api/script": {"source": "1 + 2"}, "/api/sched": {"kind": "task", "op": "add", "task": {"id": "map", "script": "1", "every_s": 86400}},
        "/api/auth": {"pin": ""}, "/api/reboot": {}, "/api/factory-reset": {},
        "/mcp": {"jsonrpc": "2.0", "id": "map", "method": "ping"},
    }
    for e in endpoints:
        if e["method"] == "POST" and e["path"] in posts:
            expected = 202 if e["path"] == "/api/agent/message" else 200
            http_case(rec, sim, "advertised-POST:" + e["path"], "POST", e["path"], posts[e["path"]], expected)
    for path, mime in [("/", "text/html"), ("/assets/app.js", "application/javascript"), ("/assets/style.css", "text/css")]:
        reply = sim.request("GET", path)
        rec.check("portal-mime:" + path, reply.headers.get("content-type", "").startswith(mime))
        rec.equal("portal-no-stale-cache:" + path, reply.headers.get("cache-control"), "no-store")
    http_case(rec, sim, "unknown-api", "GET", "/api/not-a-route", status=404)
    http_case(rec, sim, "wrong-api-method", "PATCH", "/api/config", {}, 404)
    rec.equal("captive-redirect", sim.request("GET", "/generate_204").headers.get("location"), "/")
    http_case(rec, sim, "favicon", "GET", "/favicon.ico", status=204)
    for path in ["/api/config", "/api/tools", "/api/manifest", "/api/agent/config", "/api/script", "/api/sched", "/api/wifi/connect"]:
        http_case(rec, sim, "invalid-json:" + path, "POST", path, b"{", 400)
    reply = sim.request("OPTIONS", "/mcp", headers={"Origin": "http://fixture.invalid"})
    rec.equal("cors-preflight", reply.status, 204)
    rec.equal("cors-origin", reply.headers.get("access-control-allow-origin"), "*")
    rec.check("cors-auth-headers", "X-Portal-Pin" in reply.headers.get("access-control-allow-headers", ""))
    rec.equal("PUT-config-alias", sim.request("PUT", "/api/config", {"device_name": "put-audit"}).status, 200)
    rec.equal("PUT-manifest-alias", sim.request("PUT", "/api/manifest", {"manifest": {}}).status, 200)
    before = sim.get("/api/config")
    for i, value in enumerate([None, [], 42, "settings", {"mode": "generik"}, {"mode": 12},
                               {"mode": "generic", "disabled": "oops"},
                               {"mode": "generic", "disabled": ["gpio_write", 3]}]):
        rec.equal("tool-selection-invalid:" + str(i), sim.request("POST", "/api/tools", value).status, 400)
        rec.equal("tool-selection-atomic:" + str(i), sim.get("/api/config"), before)
    rec.note("The GET/POST map is exercised, not treated as a completeness oracle; media and SSE are in their own areas. Captive paths intentionally redirect.")


def rpc(rec, sim, fake):
    init = sim.rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                  "clientInfo": {"name": "stdlib-audit", "version": "1"}}, ident="init")
    data = init.json()
    rec.equal("initialize-id", data["id"], "init")
    rec.equal("protocol-negotiation", data["result"]["protocolVersion"], "2025-06-18")
    rec.equal("protocol-header", init.headers.get("mcp-protocol-version"), "2025-06-18")
    rec.check("initialize-capabilities", data["result"]["capabilities"]["tools"]["listChanged"]
              and bool(data["result"]["instructions"]) and bool(sim.sid))
    notification = sim.request("POST", "/mcp", {"jsonrpc": "2.0", "method": "notifications/initialized"}, {"Mcp-Session-Id": sim.sid})
    rec.equal("notification-status", notification.status, 202)
    rec.equal("notification-no-body", notification.body, b"")
    session = next(s for s in sim.get("/api/sessions")["sessions"] if s["id"] == sim.sid)
    rec.equal("initialized-session", session["initialized"], True)
    for method, field in [("ping", None), ("prompts/list", "prompts"), ("resources/templates/list", "resourceTemplates"),
                          ("logging/setLevel", None), ("completion/complete", "completion")]:
        result = sim.rpc(method, {"level": "info"}).json()
        rec.check("method:" + method, result["jsonrpc"] == "2.0" and "result" in result and (field is None or field in result["result"]), short(result))
    rec.equal("unknown-method-code", sim.rpc("audit/unknown").json()["error"]["code"], -32601)
    rec.equal("missing-tool-name-code", sim.rpc("tools/call", {}).json()["error"]["code"], -32602)
    rec.equal("resource-missing-uri", sim.rpc("resources/read", {}).json()["error"]["code"], -32602)
    rec.equal("resource-unknown-uri", sim.rpc("resources/read", {"uri": "mcp://unknown"}).json()["error"]["code"], -32004)
    for case, value in [("string", "{}"), ("array", []), ("null", None), ("number", 7)]:
        result = sim.rpc("tools/call", {"name": "board", "arguments": value}).json()
        rec.check("tool-arguments-" + case + ".refused", result.get("error", {}).get("code") == -32602
                  or result.get("result", {}).get("isError") is True, short(result))
    for case, value in [("string-id", "identifier"), ("integer-id", 42), ("fractional-id", 4.5)]:
        rec.equal(case + ".echo", sim.rpc("ping", ident=value).json()["id"], value)
    before = sim.get("/api/status")["counters"]["notifications"]
    reply = sim.request("POST", "/mcp", {"jsonrpc": "2.0", "method": "audit/unknown"})
    rec.equal("unknown-notification-no-response", (reply.status, reply.body), (202, b""))
    rec.equal("notification-counter", sim.get("/api/status")["counters"]["notifications"], before + 1)
    for case, message, code in [("bad-json", b"{", -32700), ("empty-batch", [], -32600),
                                 ("scalar-message", 7, -32600), ("missing-method", {"jsonrpc": "2.0", "id": 4}, -32600),
                                 ("wrong-version", {"jsonrpc": "1.0", "id": 5, "method": "ping"}, -32600)]:
        result = sim.request("POST", "/mcp", message).json()
        rec.equal(case, result.get("error", {}).get("code"), code)
    for case, message in [("missing-jsonrpc", {"id": 1, "method": "ping"}),
                          ("object-id", {"jsonrpc": "2.0", "id": {}, "method": "ping"}),
                          ("array-params", {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": []})]:
        result = sim.request("POST", "/mcp", message).json()
        rec.check(case + ".refused", "error" in result, short(result))
    result = sim.request("POST", "/mcp", {"jsonrpc": "2.0", "id": None, "method": "ping"})
    rec.check("null-id-is-request-not-notification", result.status == 200 and result.json().get("id", MISSING) is None,
              "JSON-RPC discourages null ids but they are not absent ids; status=" + str(result.status))
    ping = {"jsonrpc": "2.0", "id": "batch", "method": "ping"}
    for case, batch, expected in [("singleton-batch-array", [ping], ["batch"]),
                                  ("mixed-notification-batch", [ping, {"jsonrpc": "2.0", "method": "ping"}, {**ping, "id": 2}], ["batch", 2])]:
        result = sim.request("POST", "/mcp", batch).json()
        rec.check(case, isinstance(result, list) and [v["id"] for v in result] == expected, short(result))
    result = sim.request("POST", "/mcp", [ping, 7, {**ping, "id": 2}]).json()
    rec.check("invalid-batch-member-does-not-discard-valid-peers", isinstance(result, list)
              and len(result) == 3 and [v.get("id") for v in result] == ["batch", None, 2], short(result))
    result = sim.request("POST", "/mcp", [{"jsonrpc": "2.0", "method": "ping"}])
    rec.equal("all-notification-batch", (result.status, result.body), (202, b""))
    resources = sim.rpc("resources/list").json()["result"]["resources"]
    rec.check("resource-list-unique", len({r["uri"] for r in resources}) == len(resources) and len(resources) >= 6)
    for resource in resources:
        uri = resource["uri"]
        result = sim.rpc("resources/read", {"uri": uri}).json()["result"]["contents"]
        rec.check("resource:" + uri, len(result) == 1 and result[0]["uri"] == uri
                  and result[0]["mimeType"] == "application/json" and isinstance(json.loads(result[0]["text"]), (dict, list)))
    one = sim.rpc("tools/list", ident="ordinary").json()["result"]
    batch = sim.request("POST", "/mcp", [{"jsonrpc": "2.0", "id": "list", "method": "tools/list"}, ping]).json()
    rec.equal("streamed-and-batch-list-equivalence", batch[0]["result"], one)
    http_case(rec, sim, "GET-MCP-refused", "GET", "/mcp", status=405)
    http_case(rec, sim, "PUT-MCP-refused", "PUT", "/mcp", {}, 405)
    old = sim.sid
    http_case(rec, sim, "DELETE-session", "DELETE", "/mcp", status=204, headers={"Mcp-Session-Id": old})
    rec.check("deleted-session-gone", all(s["id"] != old for s in sim.get("/api/sessions")["sessions"]))
    sim.post("/api/config", {"mcp": {"max_sessions": 2}})
    for n in range(3):
        sim.request("POST", "/mcp", {"jsonrpc": "2.0", "id": n, "method": "ping"})
    rec.equal("session-cap", len(sim.get("/api/sessions")["sessions"]), 2)
    sim.post("/api/config", {"mcp": {"enabled": False}})
    rec.equal("MCP-service-disabled", sim.rpc("ping").status, 503)
    rec.equal("portal-survives-disabled-MCP", sim.request("GET", "/healthz").status, 200)
    sim.post("/api/config", {"mcp": {"enabled": True, "path": "/fixture-mcp"}})
    doc = sim.get("/agent.json")
    rec.equal("live-MCP-path-discovery", doc["dynamic"]["mcp"]["path"], "/fixture-mcp")
    rec.equal("new-MCP-path-call", sim.request("POST", "/fixture-mcp", ping).json()["result"], {})
    rec.note("TTL expiry (minimum 30 s) is not waited out; allocation, reuse, initialization, deletion and the configured session cap are exercised.")


def tools(rec, sim, fake):
    specs = sim.rpc("tools/list").json()["result"]["tools"]
    names = [t["name"] for t in specs]
    portal = sim.get("/api/tools")
    catalog, devices = sim.get("/api/catalog"), sim.get("/api/devices")
    rec.inventory["default"] = {"visible": names,
        "hidden": [{"name": t["name"], "why": t.get("why"), "missing": t.get("missing", [])}
                   for t in portal["tools"] if not t["visible"]],
        "detected_devices": devices, "catalog_kinds_not_on_default_board": [d["kind"] for d in catalog
            if d["kind"] not in {"ssd1306", "bme280", "sht3x", "ina219", "led", "buttons"}]}
    rec.equal("stable-alphabetical-order", names, sorted(set(names)))
    rec.equal("portal-MCP-visible-set", sorted(t["name"] for t in portal["tools"] if t["visible"]), names)
    rec.equal("status-visible-count", sim.get("/api/status")["mcp"]["tools_visible"], len(names))
    rec.check("advertised-schemas", all(t["inputSchema"]["type"] == "object" and t.get("description") for t in specs))
    safe = {"board": {"command": "status"}, "bus": {"command": "i2c_scan"}, "manifest": {"command": "get"},
            "display": {"command": "oled_text", "arguments": {"text": "AUDIT", "line": 0}},
            "indicators": {"command": "led_set", "arguments": {"color": "#002010"}},
            "schedule": {"action": "list"}, "script": {"source": "1 + 2"}, "shell": {"command": "echo fixture"},
            "write": {"path": "/tool.txt", "text": "fixture"}, "read": {"path": "/tool.txt"},
            "edit": {"path": "/tool.txt", "find": "fixture", "replace": "changed"},
            "device_invoke": {"device": "bme280", "tool": "bme280_read", "arguments": {}}}
    # Writes precede read/edit, regardless of the advertised alphabetic order.
    for name in ["write", "read", "edit"] + [n for n in names if n not in {"write", "read", "edit"}]:
        rec.check("safe-fixture-exists:" + name, name in safe or next(t for t in specs if t["name"] == name)["inputSchema"].get("required", []) == [], name)
        success(rec, sim, "enabled:" + name, name, safe.get(name, {}))
    commands = {
        "board": [("inventory", {}), ("config_get", {}), ("config_set", {"patch": {"device_name": "tool-audit"}}),
                  ("chip_temp", {}), ("logs_tail", {"n": 2}), ("delay", {"ms": 1})],
        "bus": [("gpio_mode", {"pin": 12, "mode": "output"}), ("gpio_write", {"pin": 12, "level": 1}),
                ("gpio_read", {"pin": 12}), ("adc_read", {"pin": 1}), ("i2c_read", {"addr": 118, "reg": 208, "length": 1}),
                ("i2c_write", {"addr": 60, "data": [0, 174]}), ("i2c_identify", {"addr": 118})],
        "display": [("oled_clear", {}), ("oled_text", {"text": "CHECK", "line": 0}), ("oled_read", {}),
                    ("oled_rect", {"x": 0, "y": 16, "w": 2, "h": 2, "fill": True}),
                    ("oled_bitmap", {"data_b64": base64.b64encode(bytes(512)).decode(), "width": 128, "height": 32}),
                    ("oled_contrast", {"value": 100})],
        "indicators": [("buttons_read", {}), ("button_wait", {"timeout_ms": 10}),
                       ("led_blink", {"color": "#000000", "times": 1, "ms": 20})],
    }
    for family in ["board", "bus", "display", "indicators", "manifest"]:
        help_result = success(rec, sim, family + ".help", family, {"command": "help"})
        declared = next(t for t in specs if t["name"] == family)["inputSchema"]["properties"]["command"]["enum"]
        rec.equal(family + ".help-enum-agreement", sorted(help_result["commands"]), sorted(c for c in declared if c != "help"))
        for command, args in commands.get(family, []):
            value = success(rec, sim, family + ":" + command, family, {"command": command, "arguments": args})
            if command == "i2c_read":
                rec.equal("BME-chip-id", value["bytes"], [96])
            if command == "oled_read":
                rec.check("OLED-write-read-roundtrip", "CHECK" in short(value), short(value))
    bad = [("unknown-tool", "does_not_exist", {}), ("missing-command", "bus", {}),
           ("unknown-command", "bus", {"command": "not_real"}),
           ("nested-arguments-type", "bus", {"command": "gpio_read", "arguments": []}),
           ("pin-range", "bus", {"command": "gpio_read", "arguments": {"pin": 49}}),
           ("mode-enum", "bus", {"command": "gpio_mode", "arguments": {"pin": 12, "mode": "sideways"}}),
           ("i2c-length-range", "bus", {"command": "i2c_read", "arguments": {"addr": 118, "length": 65}}),
           ("bad-led-color", "indicators", {"command": "led_set", "arguments": {"color": "green"}}),
           ("OLED-coordinate", "display", {"command": "oled_rect", "arguments": {"x": 128, "y": 0, "w": 1, "h": 1}}),
           ("missing-device", "device_invoke", {"device": "absent", "tool": "bme280_read"}),
           ("wrong-device-action", "device_invoke", {"device": "bme280", "tool": "oled_clear"}),
           ("reset-needs-confirm", "board", {"command": "factory_reset"}),
           ("manifest-reset-needs-confirm", "manifest", {"command": "reset"}),
           ("model-cannot-reboot", "reboot", {}), ("image-needs-attachment", "image_read", {})]
    for case, name, args in bad:
        refusal(rec, sim, case, name, args)
    # Schema-required fields/types must not silently become GPIO0/level0 or byte0.
    for case, args in [("required-pin", {"level": 1}), ("required-level", {"pin": 12}),
                       ("wrong-pin-type", {"pin": [], "level": 1}), ("wrong-level-type", {"pin": 12, "level": {}})]:
        refusal(rec, sim, "schema:" + case, "bus", {"command": "gpio_write", "arguments": args})
    refusal(rec, sim, "schema:i2c-byte-range", "bus", {"command": "i2c_write", "arguments": {"addr": 60, "data": [256, -1, "bad"]}})
    for case, args in [("missing-pin-read", {}), ("float-pin-read", {"pin": 1.5}),
                       ("boolean-pin-read", {"pin": True}), ("wrong-optional-length", {"addr": 118, "length": "1"})]:
        command = "i2c_read" if case == "wrong-optional-length" else "gpio_read"
        refusal(rec, sim, "schema:" + case, "bus", {"command": command, "arguments": args})
    for name, args in [("mpu6050_read", {}), ("ads1115_read", {"channel": 0}),
                       ("lcd_text", {"text": "ABSENT", "line": 0}), ("ds18b20_read", {})]:
        rec.check("hardware-only-not-advertised:" + name, name not in names)
        refusal(rec, sim, "absent-hardware-refused:" + name, name, args)
    with Simulator(sim.binary, sim.directory / "empty", rec, empty=True) as empty:
        empty_names = empty.names()
        rec.inventory["empty"] = {"visible": empty_names, "detected_devices": empty.get("/api/devices")}
        for name in ["display", "bme280_read", "sht3x_read", "power_read"]:
            rec.check("empty-board-not-advertised:" + name, name not in empty_names)
            refusal(rec, empty, "empty-board-refused:" + name, name,
                    {"command": "oled_text", "arguments": {"text": "ABSENT"}} if name == "display" else {})
        rec.equal("empty-board-portal-alive", empty.request("GET", "/healthz").status, 200)
    sim.post("/api/tools", {"mode": "generic"})
    rec.check("generic-hides-per-device-tools", "bme280_read" not in sim.names() and "device_invoke" in sim.names())
    success(rec, sim, "generic-device-invoke", "device_invoke", safe["device_invoke"])
    sim.post("/api/tools", {"mode": "all"})
    rec.equal("all-mode-live", sim.get("/api/tools")["mode"], "all")
    rec.note("All initially advertised tools and every bus/display/indicator command above ran on VirtualBoard. The family reset commands are refusal-tested, not destructive tool invocations. Other driver hardware is not inferred from unit-test coverage.")


def permissions(rec, sim, fake):
    pin = str(secrets.randbelow(10000)).zfill(4)
    token = secrets.token_hex(16)
    sim.post("/api/config", {"auth": {"portal_pin": pin, "mcp_token": token},
                             "wifi": {"password": token, "ap_password": token}})
    credential = {"X-Portal-Pin": pin}
    for path in ["/api/config", "/api/agent/history", "/api/agent/prompt", "/api/agent/events", "/api/agent/stream", "/api/logs", "/api/script/missing", "/api/files", "/api/files/tree", "/api/files/read", "/api/attachment/x"]:
        rec.equal("private-read:" + path, sim.request("GET", path).status, 401)
    for path in ["/healthz", "/agent.json", "/api/status", "/api/tools", "/api/sched"]:
        rec.equal("public-read:" + path, sim.request("GET", path).status, 200)
    for path in ["/api/config", "/api/tools", "/api/manifest", "/api/script", "/api/agent/reset", "/api/sched/run", "/api/factory-reset"]:
        rec.equal("mutation-auth:" + path, sim.request("POST", path, {}).status, 401)
    rec.equal("wrong-PIN", sim.request("POST", "/api/auth", {"pin": pin + "x"}).status, 401)
    login = sim.post("/api/auth", {"pin": pin})
    rec.equal("portal-token-lifetime", login["expires_in"], 43200)
    rec.equal("minted-token-authorizes", sim.request("GET", "/api/config", headers={"Authorization": "Bearer " + login["token"]}).status, 200)
    url_token = urllib.parse.quote(login["token"], safe="")
    rec.equal("minted-URL-token-authorizes-read", sim.request("GET", "/api/config?token=" + url_token).status, 200)
    rec.equal("URL-token-cannot-authorize-write", sim.request("POST", "/api/agent/reset?token=" + url_token, {}).status, 401)
    rec.equal("raw-PIN-is-not-URL-token", sim.request("GET", "/api/config?token=" + pin).status, 401)
    rec.equal("forged-token-refused", sim.request("GET", "/api/config", headers={"Authorization": "Bearer fabricated"}).status, 401)
    rec.equal("query-PIN-authorizes", sim.request("GET", "/api/config?pin=" + pin).status, 200)
    public = sim.get("/api/config", credential)
    rec.equal("redacted-config-secrets", [public["auth"]["portal_pin"], public["auth"]["mcp_token"], public["wifi"]["password"], public["wifi"]["ap_password"]], ["__set__"] * 4)
    rec.check("discovery-no-secrets", token.encode() not in encode(sim.get("/agent.json")) and pin not in short(sim.get("/agent.json")["dynamic"]["security"]))
    denied = sim.rpc("ping")
    rec.equal("MCP-bearer-required", denied.status, 401)
    rec.equal("MCP-auth-challenge", denied.headers.get("www-authenticate"), "Bearer")
    rec.equal("portal-token-not-MCP-token", sim.rpc("ping", headers={"Authorization": "Bearer " + login["token"]}).status, 401)
    rec.equal("MCP-bearer-authorizes", sim.rpc("ping", headers={"Authorization": "Bearer " + token}).status, 200)
    cfg = sim.rpc("resources/read", {"uri": "mcp://config"}, headers={"Authorization": "Bearer " + token}).json()
    rec.check("MCP-config-resource-no-secrets", token not in cfg["result"]["contents"][0]["text"], short(cfg))
    cfg = sim.rpc("tools/call", {"name": "board", "arguments": {"command": "config_get"}}, headers={"Authorization": "Bearer " + token}).json()
    rec.check("MCP-config-tool-no-secrets", token.encode() not in encode(cfg), short(cfg))
    # DELETE is also part of the MCP transport, not an unauthenticated side door.
    rec.equal("MCP-session-delete-needs-bearer", sim.request("DELETE", "/mcp", headers={"Mcp-Session-Id": sim.sid}).status, 401)
    sim.post("/api/config", {"auth": {"mcp_token": ""}}, credential)
    rec.equal("MCP-unset-token-keeps-portal-gate", sim.rpc("ping").status, 401)
    rec.equal("MCP-portal-fallback-authorizes", sim.rpc("ping", headers={"Authorization": "Bearer " + login["token"]}).status, 200)
    sim.post("/api/config", {"auth": {"portal_pin": "", "mcp_token": ""}}, credential)
    for disabled, name, arguments in [
        ("write", "write", {"path": "/denied.txt", "text": "denied"}),
        ("fs_write", "fs_write", {"path": "/denied.txt", "text": "denied"}),
        ("fs_write", "write", {"path": "/denied.txt", "text": "denied"}),
        ("gpio_write", "gpio_write", {"pin": 12, "level": 1}),
        ("gpio_write", "bus", {"command": "gpio_write", "arguments": {"pin": 12, "level": 1}}),
        ("bme280_read", "device_invoke", {"device": "bme280", "tool": "bme280_read", "arguments": {}}),
        ("shell", "shell", {"command": "echo denied"}),
        ("script", "script", {"source": "1 + 2"}),
    ]:
        sim.post("/api/tools", {"disabled": [disabled]})
        rec.check("hidden:" + disabled + ":" + name, disabled not in sim.names())
        refusal(rec, sim, "dispatch-denied:" + disabled + ":" + name, name, arguments)
        if disabled != "script":
            source = "tool(" + json.dumps(name) + ", " + encode(arguments).decode() + ")"
            refusal(rec, sim, "nested-script-denied:" + disabled + ":" + name, "script", {"source": source})
    sim.post("/api/tools", {"disabled": ["fs_write"]})
    refusal(rec, sim, "shell-write-alias-denied", "shell", {"command": "tee /denied-shell.txt --text denied"})
    sim.post("/api/tools", {"disabled": []})
    success(rec, sim, "reenabled-tool", "write", {"path": "/allowed.txt", "text": "allowed"})
    rec.note("Generated in-memory PIN/tokens are only sent to the private simulator. Disabled aliases, family children, device_invoke and nested script dispatch are security regressions, never xfails.")


def files(rec, sim, fake):
    text = "alpha café 中文 🧪\nrepeat\nunique=1\nrepeat"
    wrote = success(rec, sim, "UTF8-write", "write", {"path": "audit//./notes.txt", "text": text})
    rec.equal("normalized-write-path", wrote["path"], "/audit/notes.txt")
    rec.equal("UTF8-byte-count", wrote["size_bytes"], len(text.encode()))
    rec.equal("alias-read", success(rec, sim, "read-alias", "read", {"path": "/audit/notes.txt"})["text"], text)
    query = urllib.parse.urlencode({"path": "/audit/notes.txt"})
    rec.equal("HTTP-file-read", sim.get("/api/files/read?" + query)["text"], text)
    for case, args in [("ambiguous-edit", {"find": "repeat", "replace": "wrong"}), ("missing-match", {"find": "not present", "replace": "wrong"}),
                       ("empty-find", {"find": "", "replace": "wrong"})]:
        refusal(rec, sim, case, "edit", {"path": "/audit/notes.txt", **args})
    rec.equal("failed-edits-preserve-file", sim.get("/api/files/read?" + query)["text"], text)
    success(rec, sim, "unique-edit", "edit", {"path": "/audit/notes.txt", "find": "unique=1", "replace": "unique=2"})
    text = text.replace("unique=1", "unique=2")
    for case, path in [("traversal", "/../escape"), ("backslash", "/audit\\bad"), ("control", "/audit/\u0001bad"), ("root", "/")]:
        refusal(rec, sim, "bad-path:" + case, "write", {"path": path, "text": "x"})
    refusal(rec, sim, "wrong-text-type", "write", {"path": "/wrong.txt", "text": []})
    refusal(rec, sim, "missing-path", "read", {})
    success(rec, sim, "empty-file", "write", {"path": "/audit/empty.txt", "text": ""})
    success(rec, sim, "nested-file", "write", {"path": "/audit/deep/item.txt", "text": "中文 alpha"})
    listing = sim.get("/api/files?path=/audit")
    rec.equal("directories-before-files", [r["name"] for r in listing["entries"]], ["deep", "empty.txt", "notes.txt"])
    tree = sim.get("/api/files/tree")
    rec.equal("tree-total-from-file-rows", tree["total_bytes"], sum(e.get("size_bytes", 0) for e in tree["entries"]))
    grep = success(rec, sim, "recursive-grep", "fs_grep", {"pattern": "中文", "path": "/audit"})
    rec.equal("recursive-grep-paths", sorted(m["path"] for m in grep["matches"]), ["/audit/deep/item.txt", "/audit/notes.txt"])
    regex = success(rec, sim, "regex-grep", "fs_grep", {"pattern": "^unique=[0-9]+$", "path": "/audit", "regex": True})
    rec.equal("regex-line-number", [(m["line"], m["text"]) for m in regex["matches"]], [(3, "unique=2")])
    copied = success(rec, sim, "copy-tree", "fs_cp", {"from": "/audit", "to": "/copy"})
    rec.equal("copy-files", copied["files"], 3)
    refusal(rec, sim, "move-needs-confirm", "fs_mv", {"from": "/copy", "to": "/moved"})
    success(rec, sim, "move-tree", "fs_mv", {"from": "/copy", "to": "/moved", "confirm": True})
    rec.equal("moved-content", sim.get("/api/files/read?path=/moved/notes.txt")["text"], text)
    rec.equal("moved-source-gone", sim.get("/api/files/read?path=/copy/notes.txt")["ok"], False)
    refusal(rec, sim, "copy-recursion", "fs_cp", {"from": "/audit", "to": "/audit/inner"})
    refusal(rec, sim, "remove-needs-confirm", "fs_rm", {"path": "/moved/empty.txt"})
    success(rec, sim, "remove-file", "fs_rm", {"path": "/moved/empty.txt", "confirm": True})
    refusal(rec, sim, "remove-missing", "fs_rm", {"path": "/moved/empty.txt", "confirm": True})
    cap = tree["file_max"]
    large = "界" * (cap // 3) + "x" * (cap % 3)
    success(rec, sim, "file-at-byte-ceiling", "write", {"path": "/ceiling.txt", "text": large})
    refusal(rec, sim, "file-past-byte-ceiling", "write", {"path": "/over.txt", "text": large + "x"})
    rec.equal("oversized-write-left-no-file", sim.get("/api/files/read?path=/over.txt")["ok"], False)
    rec.equal("file-ceiling-exact", sim.get("/api/files/read?path=/ceiling.txt")["size_bytes"], cap)
    sim.restart()
    rec.equal("persisted-UTF8-edit", sim.get("/api/files/read?" + query)["text"], text)
    rec.equal("persisted-large-file", sim.get("/api/files/read?path=/ceiling.txt")["text"], large)
    rec.equal("persisted-removal", sim.get("/api/files/read?path=/moved/empty.txt")["ok"], False)
    rec.note("Per-file byte limit, UTF-8, canonical paths, edits/transfers/deletion and actual process restart were exercised. Filling the entire 4 MiB workspace/PSRAM exhaustion is not claimed by this bounded live suite.")


def manifest(rec, sim, fake):
    schema = sim.get("/api/manifest/schema")
    rec.check("manifest-schema-and-primitives", isinstance(schema.get("manifest_schema"), dict) and bool(schema.get("primitives")))
    tool = {"name": "audit_level", "description": "Safe virtual GPIO12 level", "params": [{"name": "level", "type": "integer", "min": 0, "max": 1, "required": True}],
            "exec": {"type": "gpio_write", "pin": 12, "level": {"param": "level"}}, "safety": {"confirm": True}}
    document = {"version": 1, "tools": [tool]}
    before = sim.get("/api/manifest")
    dry = sim.request("POST", "/api/manifest", {"manifest": document, "mode": "merge", "dry_run": True})
    rec.equal("dry-run-report", dry.json().get("dry_run"), True)
    rec.equal("dry-run-no-change", sim.get("/api/manifest"), before)
    rec.check("dry-run-no-tool", "audit_level" not in sim.names())
    applied = http_case(rec, sim, "live-apply", "POST", "/api/manifest", {"manifest": document, "mode": "merge"})
    rec.equal("revision-moved", applied["revision"], before["revision"] + 1)
    rec.equal("no-restart", applied["restart_required"], False)
    rec.check("new-tool-discovered", "audit_level" in sim.names())
    refusal(rec, sim, "manifest-confirmation", "audit_level", {"level": 1})
    for case, value in [("range", 2), ("type", []), ("missing", MISSING)]:
        args = {"confirm": True}
        if value is not MISSING:
            args["level"] = value
        refusal(rec, sim, "manifest-argument:" + case, "audit_level", args)
    success(rec, sim, "manifest-invocation", "audit_level", {"level": 1, "confirm": True})
    rec.equal("virtual-pin-pullup-read", success(rec, sim, "manifest-pin-read", "gpio_read", {"pin": 12})["level"], 1)
    url = fake.fixture("/manifest-agent", [model_reply(text="", calls=[("live-manifest", "audit_level", {"level": 0, "confirm": True})]), model_reply()], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url, "model": "fixture", "max_steps": 3})
    hist, _ = run_turn(sim, "invoke newly installed virtual tool")
    rec.check("live-manifest-in-internal-model-schema", any(t["function"]["name"] == "audit_level" for t in fake.requests("/manifest-agent")[0]["body"]["tools"]))
    rec.check("live-manifest-internal-tool-success", any(m.get("tool_call_id") == "live-manifest" and not m.get("is_error") for m in hist["messages"]))
    rec.check("live-manifest-internal-result-pairing", paired(hist["messages"]))
    written = next(m for m in hist["messages"] if m.get("tool_call_id") == "live-manifest")
    rec.equal("live-manifest-internal-write-ack", json.loads(written["text"]), {"pin": 12, "level": 0})
    for case, doc in [("wrong-top-type", []), ("wrong-tools-type", {"tools": {}}), ("missing-exec", {"tools": [{"name": "oops"}]}),
                      ("unknown-field", {"typo": 1}), ("reserved-name", {"tools": [{"name": "board", "exec": {"type": "gpio_read", "pin": 0}}]}),
                      ("bad-pin", {"tools": [{"name": "invalid", "exec": {"type": "gpio_read", "pin": 99}}]}),
                      ("duplicate-name", {"tools": [tool, tool]}), ("unknown-primitive", {"tools": [{"name": "invalid", "exec": {"type": "nonsense"}}]})]:
        old = sim.get("/api/manifest")
        response = sim.request("POST", "/api/manifest", {"manifest": doc})
        rec.check("refused:" + case, response.status in {400, 422} and not response.json().get("applied"), short(response.json()))
        rec.equal("refusal-is-atomic:" + case, sim.get("/api/manifest"), old)
    too_many = {"tools": [{"name": "audit_n" + str(n), "exec": {"type": "gpio_read", "pin": 0}} for n in range(49)]}
    rec.equal("max-48-manifest-tools", sim.request("POST", "/api/manifest", {"manifest": too_many}).status, 422)
    rate = {"name": "audit_rate", "exec": {"type": "gpio_read", "pin": 0}, "safety": {"max_calls_per_min": 1}}
    sim.post("/api/manifest", {"mode": "merge", "manifest": {"tools": [rate]}})
    success(rec, sim, "rate-first-call", "audit_rate")
    refusal(rec, sim, "rate-second-call", "audit_rate")
    sim.restart()
    rec.check("persisted-manifest-tool", "audit_level" in sim.names())
    success(rec, sim, "persisted-tool-invocation", "audit_level", {"level": 0, "confirm": True})
    sim.post("/api/manifest", {"manifest": {}})
    rec.check("replace-removes-tools-live", "audit_level" not in sim.names() and "audit_rate" not in sim.names())
    refusal(rec, sim, "removed-tool-refused", "audit_level", {"level": 1, "confirm": True})


def scripts(rec, sim, fake):
    for section in ["overview", "builtins", "examples"]:
        value = success(rec, sim, "reference:" + section, "script", {"section": section})
        rec.equal("reference-section:" + section, value["section"], section)
    lint = success(rec, sim, "lint-does-not-execute", "script", {"lint": True, "source": 'tool("write", {path: "/lint.txt", text: "bad"})'})
    rec.equal("lint-valid", lint["ok"], True)
    rec.equal("lint-no-side-effect", sim.get("/api/files/read?path=/lint.txt")["ok"], False)
    lint = content(sim.call("script", {"lint": True, "source": "let = 3"}))
    rec.check("lint-line-and-column", lint.get("ok") is False and "line 1" in short(lint) and "column" in short(lint), short(lint))
    for case, source, expected in [("precedence", "1 + 2 * 3", 7),
                                  ("loop-and-assignment", "let n = 0\nwhile n < 3 { n = n + 1 }\nn", 3),
                                  ("arguments", "args.value + 2", 5),
                                  ("integer-roundtrip", 'tool("gpio_read", {pin: 12}).pin', 12)]:
        value = success(rec, sim, case, "script", {"source": source, "args": {"value": 3}})
        rec.equal(case + ".value", value.get("result"), expected)
    value = success(rec, sim, "stdout", "script", {"source": 'print("中文"); echo("two"); 4'})
    rec.equal("stdout-lines", value["output"], ["中文", "two"])
    refusal(rec, sim, "recursion-refused", "script", {"source": 'tool("script", {source: "1"})'})
    refusal(rec, sim, "nested-reboot-refused", "script", {"source": 'tool("reboot", {})'})
    refusal(rec, sim, "unknown-builtin", "script", {"source": "unknown_function()"})
    refusal(rec, sim, "source-ceiling", "script", {"source": "#" + "x" * 8192})
    refusal(rec, sim, "log-line-ceiling", "script", {"source": 'let i = 0; while i < 65 { print("x"); i = i + 1 }'})
    http_case(rec, sim, "HTTP-missing-source", "POST", "/api/script", {}, 400)
    http_case(rec, sim, "HTTP-source-size", "POST", "/api/script", {"source": "#" + "x" * 8192}, 413)
    inline = http_case(rec, sim, "inline", "POST", "/api/script", {"source": 'print("inline"); 6 * 7'})
    rec.equal("inline-result-output", (inline["state"], inline["result"], inline["output"]), ("done", 42, ["inline"]))
    bad = sim.post("/api/script", {"source": "let = 3"})
    rec.check("inline-syntax-error", bad.get("ok") is False and bad.get("state") == "failed" and "line" in bad.get("error", ""), short(bad))
    queued = sim.post("/api/script", {"source": 'print("background"); 7 * 8', "async": True})
    rec.check("async-job-handle", queued.get("state") in {"queued", "running", "done"} and queued.get("poll") == "/api/script/" + queued["id"])
    try:
        done = wait_for(lambda: sim.get(queued["poll"]), lambda j: j["state"] in {"done", "failed"}, seconds=3)
    except DeadlineExpired:
        raise
    except TimeoutError:
        done = sim.get(queued["poll"])
    rec.equal("background-result", (done["state"], done["result"], done["output"]), ("done", 56, ["background"]))
    http_case(rec, sim, "unknown-job", "GET", "/api/script/s999999", status=404)
    first = sim.post("/api/script", {"source": "1"})["id"]
    for n in range(4):
        sim.post("/api/script", {"source": str(n)})
    rec.equal("job-history-bounded-to-four", sim.request("GET", "/api/script/" + first).status, 404)
    value = success(rec, sim, "shell-pipeline", "shell", {"command": "echo alpha | tr a A | wc"})
    rec.check("pipeline-stdio", len(value.get("stages", [])) == 3 and bool(value.get("stdout")), short(value))
    success(rec, sim, "shell-redirection", "shell", {"command": "echo first > /redirect.txt"})
    success(rec, sim, "shell-append", "shell", {"command": "echo second >> /redirect.txt"})
    rec.equal("redirected-file", sim.get("/api/files/read?path=/redirect.txt")["text"], "first\nsecond\n")
    for case, command in [("six-stages", "echo x | cat | cat | cat | cat | cat"),
                          ("command-substitution", "echo $(id)"), ("shell-traversal", "echo x > /../escape")]:
        refusal(rec, sim, case, "shell", {"command": command})
    rec.note("Runaway wall-time/5-million-step and 30-second sleep budgets are unit-tested, not spent again live; sync, background, lint, stdout, bounded job history and dispatch are exercised here.")


def sched(rec, sim, fake):
    rec.equal("initial-schedule", (sim.get("/api/sched")["tasks"], sim.get("/api/sched")["rules"]), ([], []))
    task = {"id": "audit-task", "prompt": "fixture scheduled prompt", "script": 'print("scheduled"); 20 + 22', "every_s": 86400}
    added = sim.post("/api/sched", {"kind": "task", "op": "add", "task": task})
    rec.equal("task-added", added["tasks"][0]["id"], "audit-task")
    clock = added["tasks"][0]["last_run_ms"]
    rec.check("new-task-clock-not-epoch", clock > 1700000000000)
    for op in ["disable", "enable"]:
        result = sim.post("/api/sched", {"kind": "task", "op": op, "id": task["id"]})
        rec.equal("task-" + op, result["tasks"][0]["enabled"], op == "enable")
    run = sim.post("/api/sched/run", {"id": task["id"]})
    rec.equal("script-wins-over-prompt", run["kind"], "script")
    job = sim.get("/api/script/" + run["job"])
    rec.equal("manual-script-result", (job["result"], job["output"]), (42, ["scheduled"]))
    rec.equal("manual-run-keeps-clock", sim.get("/api/sched")["tasks"][0]["last_run_ms"], clock)
    for case, body in [("unknown-kind", {"kind": "unknown", "op": "add"}),
                       ("no-task-action", {"kind": "task", "op": "add", "task": {"every_s": 1}}),
                       ("no-trigger", {"kind": "task", "op": "add", "task": {"prompt": "x"}}),
                       ("invalid-clock", {"kind": "task", "op": "add", "task": {"prompt": "x", "at_hhmm": "24:00"}}),
                       ("bad-rule-op", {"kind": "rule", "op": "add", "rule": {"tool": "bme280_read", "op": "approx", "prompt": "x"}}),
                       ("rule-no-prompt", {"kind": "rule", "op": "add", "rule": {"tool": "bme280_read"}}),
                       ("rule-network-refused", {"kind": "rule", "op": "add", "rule": {"tool": "net_get", "prompt": "x"}})]:
        http_case(rec, sim, case, "POST", "/api/sched", body, 400)
    http_case(rec, sim, "unknown-task-run", "POST", "/api/sched/run", {"id": "missing"}, 404)
    http_case(rec, sim, "unknown-task-delete", "POST", "/api/sched", {"kind": "task", "op": "delete", "id": "missing"}, 404)
    sim.restart()
    rec.equal("task-persists", sim.get("/api/sched")["tasks"][0]["script"], task["script"])
    sim.post("/api/sched", {"kind": "task", "op": "delete", "id": task["id"]})
    rec.equal("task-deleted", sim.get("/api/sched")["tasks"], [])
    rule = {"id": "audit-rule", "tool": "bme280_read", "path": "temperature_c", "op": "gt", "value": 999,
            "prompt": "fixture rule prompt", "enabled": False, "min_interval_s": 86400}
    added = sim.post("/api/sched", {"kind": "rule", "op": "add", "rule": rule})
    rec.equal("sensor-rule-added", added["rules"][0]["id"], rule["id"])
    for op in ["enable", "disable"]:
        result = sim.post("/api/sched", {"kind": "rule", "op": op, "id": rule["id"]})
        rec.equal("sensor-rule-" + op, result["rules"][0]["enabled"], op == "enable")
    updated = sim.post("/api/sched", {"kind": "rule", "op": "update", "rule": {**rule, "value": 1000}})
    rec.equal("sensor-rule-update", updated["rules"][0]["value"], 1000)
    sim.restart()
    rec.equal("sensor-rule-persists", sim.get("/api/sched")["rules"][0]["prompt"], rule["prompt"])
    sim.post("/api/sched", {"kind": "rule", "op": "delete", "id": rule["id"]})
    rec.equal("sensor-rule-deleted", sim.get("/api/sched")["rules"], [])
    http_case(rec, sim, "unknown-rule-delete", "POST", "/api/sched", {"kind": "rule", "op": "delete", "id": "missing"}, 404)
    for n in range(8):
        added = sim.request("POST", "/api/sched", {"kind": "rule", "op": "add", "rule": {**rule, "id": "rule-" + str(n)}})
        rec.equal("sensor-rule-slot:" + str(n), added.status, 200)
    rec.equal("sensor-rule-ninth-refused", sim.request("POST", "/api/sched", {"kind": "rule", "op": "add", "rule": {**rule, "id": "rule-overflow"}}).status, 400)
    rec.equal("sensor-rule-cap-eight", len(sim.get("/api/sched")["rules"]), 8)
    for n in range(8):
        sim.post("/api/sched", {"kind": "rule", "op": "delete", "id": "rule-" + str(n)})
    url = fake.fixture("/scheduled-prompt", [model_reply(text="scheduled answer")], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url, "model": "fixture"})
    prompt_task = {"id": "prompt-task", "prompt": "scheduled question", "every_s": 86400}
    sim.post("/api/sched", {"kind": "task", "op": "add", "task": prompt_task})
    after = sim.get("/api/agent/events")["seq"]
    ran = sim.post("/api/sched/run", {"id": prompt_task["id"]})
    rec.equal("manual-prompt-queued", (ran["kind"], ran["queued"], ran["source"]), ("prompt", True, "schedule:prompt-task"))
    hist, _ = finish_turn(sim, after)
    rec.equal("scheduled-prompt-source", hist["messages"][-2]["source"], "schedule:prompt-task")
    rec.equal("scheduled-prompt-answer", hist["messages"][-1]["text"], "scheduled answer")
    sim.post("/api/sched", {"kind": "task", "op": "delete", "id": prompt_task["id"]})
    timed = {"id": "timer-task", "every_s": 1, "script": 'tool("write", {path: "/timer.txt", text: "timer fired"})'}
    added = sim.post("/api/sched", {"kind": "task", "op": "add", "task": timed})
    born = added["tasks"][0]["last_run_ms"]
    rec.equal("timed-script-not-immediate", sim.get("/api/files/read?path=/timer.txt")["ok"], False)
    wait_for(lambda: sim.get("/api/files/read?path=/timer.txt"), lambda v: v.get("ok") is True, seconds=4)
    rec.equal("real-tick-runs-script", sim.get("/api/files/read?path=/timer.txt")["text"], "timer fired")
    ran = sim.get("/api/sched")["tasks"][0]["last_run_ms"]
    rec.check("real-tick-advances-clock-after-interval", ran >= born + 1000, str((born, ran)))
    sim.post("/api/sched", {"kind": "task", "op": "delete", "id": timed["id"]})
    # /api/sched advertises replacement, but the implementation currently only accepts kind/op.
    rec.equal("advertised-schedule-replacement", sim.request("POST", "/api/sched", {"tasks": [], "rules": []}).status, 200)
    one = success(rec, sim, "schedule-tool-add-one", "schedule", {"action": "add", "prompt": "one", "every_s": 86400})
    two = success(rec, sim, "schedule-tool-add-two", "schedule", {"action": "add", "prompt": "two", "every_s": 86400})
    sim.call("schedule", {"action": "remove", "id": one["id"]})
    three = success(rec, sim, "schedule-tool-add-after-removal", "schedule", {"action": "add", "prompt": "three", "every_s": 86400})
    rec.check("tool-task-ids-do-not-collide", three["id"] != two["id"], short(sim.get("/api/sched")["tasks"]))
    refusal(rec, sim, "tool-invalid-clock", "schedule", {"action": "add", "prompt": "bad clock", "at_hhmm": "25:00"})
    for n in range(9):
        response = sim.request("POST", "/api/sched", {"kind": "task", "op": "add", "task": {"id": "bounded-" + str(n), "script": "1", "every_s": 86400}})
        if response.status == 400:
            break
    rec.check("REST-task-cap-eight", len(sim.get("/api/sched")["tasks"]) <= 8)
    result = sim.call("schedule", {"action": "add", "prompt": "overflow", "every_s": 86400})
    rec.check("tool-obeys-task-cap-eight", result.get("isError") is True and len(sim.get("/api/sched")["tasks"]) <= 8, short(result))
    rec.note("Manual execution, recurring-task lifecycle, clock validation, REST/tool parity, persistence and bounds are covered. Real daily-time/SNTP/radio behavior is not simulated as hardware evidence.")


def agent(rec, sim, fake):
    for case, body in [("missing-text", {}), ("blank-text", {"text": "   "}), ("wrong-text-type", {"text": []})]:
        http_case(rec, sim, case, "POST", "/api/agent/message", body, 400)
    http_case(rec, sim, "user-byte-ceiling", "POST", "/api/agent/message", {"text": "x" * 32769}, 413)
    hist, events = run_turn(sim, "unconfigured must fail quickly")
    rec.check("unconfigured-model-error", any(e["event"] == "error" and "base_url" in e.get("text", "") for e in events), short(events))
    sim.post("/api/agent/reset", {})
    url = fake.fixture("/agent-flow", [model_reply(text="", calls=[("c1", "write", {"path": "/agent.txt", "text": "written 中文"}),
                                                                ("c2", "board", {"command": "status"})]),
                                        model_reply(text="agent complete ✓")], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url, "model": "fixture", "max_steps": 5, "max_output_tokens": 128})
    hist, events = run_turn(sim, "exercise two safe tools")
    rec.equal("multi-tool-final", hist["messages"][-1]["text"], "agent complete ✓")
    rec.check("call-result-pairing", paired(hist["messages"]), short(hist))
    rec.equal("two-tool-results", [m["tool_call_id"] for m in hist["messages"] if m["role"] == "tool"], ["c1", "c2"])
    rec.equal("tool-side-effect", sim.get("/api/files/read?path=/agent.txt")["text"], "written 中文")
    rec.equal("usage-two-model-calls", hist["usage"]["last_run"]["total_tokens"], 36)
    for ident in ["c1", "c2"]:
        relevant = [e for e in events if e.get("tool_call_id") == ident]
        rec.equal("progress-pair:" + ident, [(e["event"], e.get("phase")) for e in relevant],
                  [("tool_use", None), ("tool_progress", "start"), ("tool_result", None), ("tool_progress", "end")])
    rec.equal("journal-completed", events[-1]["event"], "run_end")
    rec.check("journal-monotonic-seq", all(a["seq"] < b["seq"] for a, b in zip(events, events[1:])))
    after = events[len(events) // 2]["seq"]
    rec.check("events-after-filter", all(e["seq"] > after for e in sim.get("/api/agent/events?after=" + str(after))["events"]))
    page = sim.get("/api/agent/history?after=2")
    rec.equal("history-page", page["messages"], hist["messages"][2:])
    url = fake.fixture("/agent-second", [model_reply(text="second answer")], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url})
    second, _ = run_turn(sim, "second question")
    sent = fake.requests("/agent-second")[0]["body"]["messages"]
    rec.check("multi-turn-context-retained", any(m.get("role") == "tool" and m.get("tool_call_id") == "c1" for m in sent))
    rec.check("multi-turn-pairs-retained", paired(second["messages"]))
    forgotten = sim.post("/api/agent/forget", {"keep": 1})
    rec.equal("forget-keeps-one-exchange", forgotten["kept"], 2)
    sim.restart()
    rec.equal("forget-persisted", [m["text"] for m in sim.get("/api/agent/history")["messages"]], ["second question", "second answer"])
    sim.post("/api/agent/reset", {})
    rec.equal("reset-clears-history", sim.get("/api/agent/history")["messages"], [])
    # Context layers carry indexes, not the bodies they point to.
    for path, text in [("/AGENTS.md", "audit project instruction"), ("/MEMORY.md", "- [fixture](/memory/f.md) — memory pointer"),
                       ("/skills/audit/SKILL.md", "---\nname: audit-skill\ndescription: audit skill index\n---\nDO-NOT-INLINE-BODY"),
                       ("/memory/f.md", "DO-NOT-INLINE-MEMORY")]:
        sim.call("write", {"path": path, "text": text})
    sim.post("/api/agent/prompt", {"action": "set", "text": "audit standing orders"})
    sim.post("/api/agent/prompt", {"action": "append", "text": "second standing line"})
    prompt = sim.get("/api/agent/prompt")
    rec.check("context-index-layers", all(t in prompt["system"] for t in ["audit standing orders", "second standing line", "audit project instruction", "memory pointer", "audit skill index"]))
    rec.check("context-does-not-inline-pointed-bodies", "DO-NOT-INLINE-BODY" not in prompt["system"] and "DO-NOT-INLINE-MEMORY" not in prompt["system"])
    rec.equal("prompt-byte-count", prompt["bytes"], len(prompt["system"].encode()))
    limits = sim.get("/agent.json")["conventions"]["limits"]
    rec.check("generated-prompt-shares-reply-budget", "reply text limit is " + str(limits["reply_bytes"]) + " UTF-8 bytes" in prompt["system"] and "2 KB is the file limit" not in prompt["system"], prompt["system"])
    sim.post("/api/agent/prompt", {"action": "clear"})
    rec.check("clear-standing-orders-only", "audit standing orders" not in sim.get("/api/agent/prompt")["system"] and "audit project instruction" in sim.get("/api/agent/prompt")["system"])
    http_case(rec, sim, "unknown-prompt-action", "POST", "/api/agent/prompt", {"action": "oops"}, 400)
    attachment = {"name": "shot 中文.png", "mime": "image/png", "data_b64": base64.b64encode(b"fixture image bytes").decode()}
    sim.post("/api/agent/config", {"base_url": url, "vision": True})
    hist, _ = run_turn(sim, "attached message", [attachment])
    path = "/api/attachment/" + urllib.parse.quote(attachment["name"])
    fetched = sim.request("GET", path)
    rec.equal("attachment-byte-roundtrip", fetched.body, b"fixture image bytes")
    rec.equal("attachment-MIME", fetched.headers.get("content-type"), "image/png")
    rec.equal("attachment-no-cache", fetched.headers.get("cache-control"), "no-store")
    rec.check("attachment-in-transcript", "shot 中文.png" in hist["messages"][-2]["text"])
    sent = fake.requests("/agent-second")[-1]["body"]["messages"][-1]["content"]
    rec.check("vision-content-parts", isinstance(sent, list) and any(p.get("type") == "image_url" for p in sent), short(sent))
    for case, attachments, status in [("too-many-attachments", [attachment] * 3, 413),
                                      ("bad-base64", [{**attachment, "data_b64": "%%%"}], 400),
                                      ("attachment-byte-ceiling", [{**attachment, "data_b64": base64.b64encode(b"x" * 24577).decode()}], 413)]:
        http_case(rec, sim, case, "POST", "/api/agent/message", {"text": "bad upload", "attachments": attachments}, status)
    # The last message without attachments must not inherit a previous message's bytes.
    run_turn(sim, "new attachment", [attachment])
    hist, _ = run_turn(sim, "now no attachment")
    rec.equal("new-text-message-clears-last-attachment", sim.request("GET", path).status, 404)
    newest = fake.requests("/agent-second")[-1]["body"]["messages"][-1]["content"]
    rec.check("new-text-message-no-stale-attachment", not isinstance(newest, list)
              or not any(p.get("type") == "image_url" for p in newest), short(newest))
    # Low context budget makes compaction observable without a 2 MiB filler run.
    url = fake.fixture("/compact", [model_reply(text="answer " + "界" * 220)], dialect="chat")
    sim.post("/api/agent/reset", {})
    sim.post("/api/agent/config", {"base_url": url, "context_tokens": 512, "max_output_tokens": 64})
    for n in range(7):
        hist, _ = run_turn(sim, "question " + str(n) + " " + "界" * 100)
    rec.check("context-compacted", sim.get("/api/agent/prompt")["archive_lines"] > 0)
    rec.check("compaction-pairing", paired(hist["messages"]))
    rec.equal("event-ring-rollover", len(sim.get("/api/agent/events")["events"]), 96)

    sim.post("/api/agent/reset", {})
    url = fake.fixture("/step-limit", [model_reply(text="", calls=[("step", "board", {"command": "status"})])], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url, "max_steps": 1, "context_tokens": 262144, "max_output_tokens": 128})
    hist, events = run_turn(sim, "bounded model loop")
    rec.equal("step-limit-one-model-request", len(fake.requests("/step-limit")), 1)
    rec.check("step-limit-error", any(e["event"] == "error" and "step limit" in e.get("text", "") for e in events), short(events))
    rec.check("step-limit-still-pairs-results", paired(hist["messages"]))

    sim.post("/api/agent/reset", {})
    sim.call("gpio_write", {"pin": 12, "level": 0})
    disabled = ["fs_write", "gpio_write", "bme280_read", "shell", "script"]
    sim.post("/api/tools", {"disabled": disabled})
    calls = [("deny-write", "write", {"path": "/internal-denied.txt", "text": "denied"}),
             ("deny-fs", "fs_write", {"path": "/internal-denied-fs.txt", "text": "denied"}),
             ("deny-bus", "bus", {"command": "gpio_write", "arguments": {"pin": 12, "level": 1}}),
             ("deny-device", "device_invoke", {"device": "bme280", "tool": "bme280_read", "arguments": {}}),
             ("deny-shell", "shell", {"command": "echo denied"}), ("deny-script", "script", {"source": "1 + 2"})]
    url = fake.fixture("/internal-denied", [model_reply(text="", calls=calls), model_reply(text="refusals acknowledged")], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url, "max_steps": 4})
    hist, _ = run_turn(sim, "a model cannot bypass disabled dispatch")
    results = {m["tool_call_id"]: m for m in hist["messages"] if m["role"] == "tool"}
    for ident, name, _ in calls:
        rec.check("internal-disabled:" + name, results.get(ident, {}).get("is_error") is True, short(results.get(ident)))
    rec.check("internal-refusals-paired", paired(hist["messages"]))
    rec.equal("internal-disabled-file-no-side-effect", sim.get("/api/files/read?path=/internal-denied.txt")["ok"], False)
    rec.equal("internal-disabled-alias-no-side-effect", sim.get("/api/files/read?path=/internal-denied-fs.txt")["ok"], False)
    # gpio_read changes mode to input/pullup; it is not output-latch feedback on VirtualBoard.
    pin_ack = json.loads(results.get("deny-bus", {}).get("text", "{}"))
    rec.check("internal-disabled-pin-no-success-ack", pin_ack.get("ok") is not True, short(pin_ack))
    sent = fake.requests("/internal-denied")[0]["body"]["tools"]
    rec.check("model-list-does-not-advertise-disabled", not any(t["function"]["name"] in disabled + ["write"] for t in sent))
    sim.post("/api/tools", {"disabled": []})

    sim.post("/api/agent/reset", {})
    url = fake.fixture("/mixed-errors", [model_reply(text="", calls=[("ok", "board", {"command": "status"}),
        ("bad", "board_status", "{bad"), ("unknown", "not_a_tool", {}), ("unnamed", "", {})]), model_reply()], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url})
    hist, _ = run_turn(sim, "valid and invalid calls in the same model reply")
    results = {m["tool_call_id"]: m for m in hist["messages"] if m["role"] == "tool"}
    rec.check("mixed-model-reply-keeps-valid-peer", not results.get("ok", {}).get("is_error") and "ok" in results)
    for ident in ["bad", "unknown", "unnamed"]:
        rec.check("mixed-model-error:" + ident, results.get(ident, {}).get("is_error") is True, short(results.get(ident)))
    rec.check("mixed-model-errors-paired", paired(hist["messages"]))

    sim.post("/api/agent/reset", {})
    big = "界" * 24000
    sim.call("write", {"path": "/large-output.txt", "text": big})
    long_argument = "é" * 1500
    url = fake.fixture("/bounded-output", [model_reply(text="", calls=[
        ("long-args", "write", {"path": "/long-args.txt", "text": long_argument}),
        ("long-result", "read", {"path": "/large-output.txt"})]),
        model_reply(text="界" * 16000, reasoning="é" * 3000)], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url})
    hist, _ = run_turn(sim, "bounded UTF-8 output")
    final = hist["messages"][-1]
    # truncate_bytes caps the prefix and appends MARK outside that prefix budget.
    rec.check("assistant-output-byte-ceiling", len(final["text"].encode()) <= 32768 + MARK_BYTES and final["text"].endswith(MARK), str(len(final["text"].encode())))
    rec.check("reasoning-byte-ceiling", len(final["reasoning"].encode()) <= 4096 + MARK_BYTES and final["reasoning"].endswith(MARK), str(len(final["reasoning"].encode())))
    result = next(m for m in hist["messages"] if m.get("tool_call_id") == "long-result")
    rec.check("tool-output-byte-ceiling", len(result["text"].encode()) <= 65536 + MARK_BYTES and result["text"].endswith(MARK), str(len(result["text"].encode())))
    call = next(c for m in hist["messages"] for c in m.get("tool_calls", []) if c["id"] == "long-args")
    rec.check("portal-elides-large-arguments-honestly", isinstance(call["arguments"], str) and call["arguments_elided_bytes"] > 0)
    outbound = fake.requests("/bounded-output")[1]["body"]["messages"]
    wire_call = next(c for m in outbound for c in m.get("tool_calls", []) if c["id"] == "long-args")
    rec.equal("portal-elision-does-not-corrupt-model-arguments", json.loads(wire_call["function"]["arguments"])["text"], long_argument)
    rec.check("bounded-output-pairing", paired(hist["messages"]))
    sim.restart()
    hist = sim.get("/api/agent/history")
    rec.check("saved-output-text-byte-ceiling", all(len(m["text"].encode()) <= 4096 + MARK_BYTES for m in hist["messages"]), short([len(m["text"].encode()) for m in hist["messages"]]))
    rec.check("saved-output-pairing", paired(hist["messages"]))

    sim.post("/api/agent/reset", {})
    url = fake.fixture("/while-thinking", [model_reply(text="first done"), model_reply(text="latest done")], dialect="chat", delay=1.2)
    sim.post("/api/agent/config", {"base_url": url})
    after = sim.get("/api/agent/events")["seq"]
    rec.equal("thinking-turn-queued", sim.request("POST", "/api/agent/message", {"text": "slow first"}).status, 202)
    wait_for(lambda: fake.requests("/while-thinking"), bool)
    rec.equal("HTTP-responsive-during-model-call", sim.request("GET", "/api/status", timeout=0.8).status, 200)
    rec.equal("second-message-accepted-while-thinking", sim.request("POST", "/api/agent/message", {"text": "obsolete pending"}).status, 202)
    rec.equal("newest-pending-message-accepted", sim.request("POST", "/api/agent/message", {"text": "latest pending"}).status, 202)
    hist, _ = finish_turn(sim, after, runs=2)
    users = [m["text"] for m in hist["messages"] if m["role"] == "user"]
    rec.equal("pending-coalescing-newest-wins", users, ["slow first", "latest pending"])
    rec.check("phase-split-multi-turn-pairing", paired(hist["messages"]))

    sim.post("/api/agent/reset", {})
    sim.post("/api/agent/config", {"base_url": sim.url + "/selftest/chat/completions", "model": "selftest", "protocol": "chat"})
    hist, _ = run_turn(sim, "status")
    rec.check("canned-selftest-dispatches-default-advertised-tool", any(m["role"] == "tool" for m in hist["messages"])
              and "the board answered" in hist["messages"][-1]["text"], short(hist))
    rec.check("canned-selftest-history-paired", paired(hist["messages"]))
    rec.note("Live phases, progress, disabled internal dispatch, multi-turn/queued coalescing, usage, forget, context indexes, attachments, compaction, output/storage ceilings and canned selftest are exercised. The full 1024-message/2 MiB history backstop, power-cut resume and real multimodal inference are not claimed.")


def transport(rec, sim, fake):
    key = secrets.token_hex(16)  # ephemeral dummy, never a provider key
    for protocol in ["chat", "responses", "anthropic"]:
        sim.post("/api/agent/reset", {})
        path = "/wire-" + protocol
        call = ("wire-call", "board", {"command": "status"})
        url = fake.fixture(path, [model_reply(protocol, text="", calls=[call]), model_reply(protocol, text=protocol + " final 中文")], dialect=protocol, mode="chunked")
        sim.post("/api/agent/config", {"base_url": url, "model": "fixture-" + protocol, "protocol": protocol,
                                      "api_key": key, "reasoning": "high", "temperature": 0.5, "max_steps": 4, "max_output_tokens": 128})
        hist, events = run_turn(sim, "wire " + protocol)
        rec.equal(protocol + ".decoded-final", hist["messages"][-1]["text"], protocol + " final 中文")
        rec.check(protocol + ".paired-history", paired(hist["messages"]))
        rec.equal(protocol + ".usage", hist["usage"]["last_run"]["total_tokens"], 36)
        requests = fake.requests(path)
        rec.equal(protocol + ".two-requests", len(requests), 2)
        first, second = requests[0]["body"], requests[1]["body"]
        heads = requests[0]["headers"]
        rec.equal(protocol + ".wire-method", requests[0]["method"], "POST")
        rec.equal(protocol + ".content-type", heads.get("content-type"), "application/json")
        if protocol == "anthropic":
            rec.equal(protocol + ".auth", (heads.get("x-api-key"), heads.get("anthropic-version"), heads.get("authorization")), (key, "2023-06-01", None))
            rec.check(protocol + ".schema-shape", isinstance(first.get("system"), str) and all("input_schema" in t and "function" not in t for t in first["tools"]))
            blocks = [b for m in second["messages"] for b in m.get("content", []) if isinstance(b, dict)]
            rec.check(protocol + ".tool-result-block", any(b.get("type") == "tool_result" and b.get("tool_use_id") == "wire-call" for b in blocks), short(blocks))
        else:
            rec.equal(protocol + ".auth", heads.get("authorization"), "Bearer " + key)
            if protocol == "responses":
                rec.check(protocol + ".schema-shape", all("name" in t and "parameters" in t and "function" not in t for t in first["tools"]))
                rec.check(protocol + ".tool-result-item", any(i.get("type") == "function_call_output" and i.get("call_id") == "wire-call" for i in second["input"]), short(second["input"]))
                rec.equal(protocol + ".reasoning", first["reasoning"], {"effort": "high"})
            else:
                rec.check(protocol + ".schema-shape", all(t.get("type") == "function" and "function" in t for t in first["tools"]))
                rec.check(protocol + ".tool-result-message", any(m.get("role") == "tool" and m.get("tool_call_id") == "wire-call" for m in second["messages"]))
                rec.equal(protocol + ".reasoning", first.get("reasoning_effort"), "high")
        rec.check(protocol + ".secret-redacted", key not in short(sim.get("/api/agent/config"), 30000))
        # image_read must use the configured dialect too, not force Chat.
        image_path = "/image-" + protocol
        image_url = fake.fixture(image_path, [model_reply(protocol, text="image fixture")], dialect=protocol)
        sim.post("/api/agent/config", {"base_url": image_url})
        attached = {"name": "fixture.png", "mime": "image/png", "data_b64": base64.b64encode(b"fixture").decode()}
        run_turn(sim, "image named fixture", [attached])
        result = sim.call("image_read", {"name": "fixture.png", "prompt": "fixture OCR"})
        rec.check(protocol + ".image-read-dialect", result.get("isError") is False and content(result).get("text") == "image fixture", short(result))
        last = fake.requests(image_path)[-1]["body"]
        if protocol == "anthropic":
            images = [p.get("source", {}) for m in last.get("messages", [])
                      for p in m.get("content", []) if isinstance(p, dict) and p.get("type") == "image"]
            present = any(isinstance(source, dict) and source.get("type") == "base64"
                          and source.get("media_type") == "image/png" and source.get("data") == "Zml4dHVyZQ=="
                          for source in images)
        else:
            present = "Zml4dHVyZQ==" in short(last, 60000)
        rec.check(protocol + ".image-payload-present", present, short(last))
    # Malformed arguments must be a paired tool error, never an execution with defaults.
    for protocol, raw_args in [("chat", "{bad"), ("responses", "{bad"), ("anthropic", []), ("chat", "[]")]:
        case = protocol + ("-array-args" if raw_args == "[]" or raw_args == [] else "-bad-args")
        sim.post("/api/agent/reset", {})
        url = fake.fixture("/" + case, [model_reply(protocol, text="", calls=[("bad-call", "board_status", raw_args)]), model_reply(protocol)], dialect=protocol)
        sim.post("/api/agent/config", {"base_url": url, "protocol": protocol})
        hist, _ = run_turn(sim, case)
        results = [m for m in hist["messages"] if m["role"] == "tool"]
        rec.check(case + ".paired-error-no-execution", len(results) == 1 and results[0].get("is_error") is True and paired(hist["messages"]), short(hist))
    for case, mode in [("close-delimited", "close"), ("informational-before-final", "interim")]:
        sim.post("/api/agent/reset", {})
        url = fake.fixture("/" + case, [model_reply(text=case)], mode=mode)
        sim.post("/api/agent/config", {"base_url": url, "protocol": "chat", "api_key": ""})
        hist, _ = run_turn(sim, case)
        rec.equal(case, hist["messages"][-1]["text"], case)
        rec.equal(case + ".no-empty-key-header", fake.requests("/" + case)[0]["headers"].get("authorization"), None)
    sim.post("/api/agent/reset", {})
    path = "/retry-success"
    url = fake.fixture(path, [{"error": "transient"}, model_reply(text="", calls=[("retry-once", "board", {"command": "status"})]),
                              model_reply(text="recovered")], statuses=[503, 200, 200], dialect="chat")
    sim.post("/api/agent/config", {"base_url": url})
    hist, events = run_turn(sim, "transient HTTP error must not duplicate tools")
    rec.equal("retry-success-final", hist["messages"][-1]["text"], "recovered")
    rec.equal("retry-success-three-wire-requests", len(fake.requests(path)), 3)
    rec.equal("retry-success-tool-called-once", sum(m.get("tool_call_id") == "retry-once" for m in hist["messages"]), 1)
    rec.check("retry-success-journal-not-error", any(e["event"] == "retried" for e in events) and not any(e["event"] == "error" for e in events), short(events))
    rec.check("retry-success-pairing", paired(hist["messages"]))
    for case, options, expected_attempts in [
        ("status-401", {"status": 401}, 1), ("status-429", {"status": 429}, 1), ("status-503", {"status": 503}, 3),
        ("non-JSON-200", {"replies": [b"not JSON"]}, 1), ("short-reply", {"mode": "short"}, None),
        ("bad-chunk-reply", {"mode": "bad-chunk"}, None), ("oversized-reply", {"mode": "oversized"}, None),
        ("duplicate-length-reply", {"mode": "duplicate-length"}, None),
    ]:
        sim.post("/api/agent/reset", {})
        path = "/error-" + case
        options = dict(options)
        replies = options.pop("replies", [model_reply(text="must not be accepted")])
        url = fake.fixture(path, replies, **options)
        sim.post("/api/agent/config", {"base_url": url})
        hist, events = run_turn(sim, case)
        rec.check(case + ".fails-loudly", any(e["event"] == "error" for e in events)
                  and not any(m["role"] == "assistant" and m["text"] == "must not be accepted" for m in hist["messages"]), short(events))
        if expected_attempts is not None:
            rec.equal(case + ".retry-count", len(fake.requests(path)), expected_attempts)
    # The configuration's minimum timeout is 5 s; delay 6 s, bounded to 22 s including retries.
    sim.post("/api/agent/reset", {})
    url = fake.fixture("/timeout", [model_reply(text="late-success")], delay=6)
    config = sim.post("/api/agent/config", {"base_url": url, "timeout_s": 5})
    rec.equal("timeout-setting", config["timeout_s"], 5)
    _, events = run_turn(sim, "timeout must be applied", seconds=22)
    rec.check("configured-model-timeout-is-enforced", any(e["event"] == "error" for e in events), short(events))
    rec.note("Chat/Responses/Anthropic request shapes, auth, tool results, image_read, chunked/close/interim decoding, errors, retries and the configured timeout use local fake endpoints. TLS, real providers, billing and provider-specific reasoning signatures are unverified.")


def network(rec, sim, fake):
    html = '<html><head><title>中文 &amp; café</title><style>HIDDEN_STYLE</style></head><body><h1>测量</h1><p>A &lt; B 🧪</p><script>HIDDEN_SCRIPT</script></body></html>'.encode()
    url = fake.fixture("/unicode", [html], mode="chunked")
    value = success(rec, sim, "Unicode-HTML-text", "net_get", {"url": url})
    rec.equal("HTML-title", value.get("title"), "中文 & café")
    rec.check("HTML-visible-text-only", "测量" in value["body"] and "A < B 🧪" in value["body"] and "HIDDEN" not in value["body"])
    value = success(rec, sim, "HTML-raw", "net_get", {"url": url, "format": "raw"})
    rec.equal("raw-keeps-markup", value["body"], html.decode())
    url = fake.fixture("/echo", [{}], echo=True)
    value = success(rec, sim, "real-GET", "net_get", {"url": url + "?q=1#fragment"})
    rec.equal("GET-query-fragment", json.loads(value["body"])["target"], "/echo?q=1")
    body = '{"text":"中文 café"}'
    value = success(rec, sim, "real-POST", "net_post", {"url": url, "body": body})
    rec.equal("POST-wire-body", json.loads(value["body"])["body"], {"text": "中文 café"})
    rec.equal("POST-byte-count", value["request_bytes"], len(body.encode()))
    rec.equal("POST-content-type", fake.requests("/echo")[-1]["headers"].get("content-type"), "application/json")
    for case, arguments in [("missing-URL", {}), ("wrong-URL-type", {"url": []}), ("bad-scheme", {"url": "ftp://127.0.0.1/x"}),
                            ("header-injection", {"url": url + "\r\nX-Injected: yes"}), ("URL-length", {"url": url + "/" + "x" * 600})]:
        refusal(rec, sim, case, "net_get", arguments)
    refusal(rec, sim, "POST-wrong-body-type", "net_post", {"url": url, "body": {}})
    # Quotes/control bytes have a large JSON-escaping cost; the entire result stays JSON.
    payload = (b'"\\\x01' * 16000) + "界".encode() * 1000
    url = fake.fixture("/escaping", [payload])
    value = success(rec, sim, "escaped-network-budget", "net_get", {"url": url, "format": "raw"})
    rec.check("escaping-truncated-with-marker", value.get("truncated") is True and value["body"].endswith(MARK), short(value))
    rec.check("serialized-result-within-64KiB", len(encode(value)) <= 65536, str(len(encode(value))))
    url = fake.fixture("/net-503", [b"fixture external error"], status=503)
    refusal(rec, sim, "network-status-preserved", "net_get", {"url": url}, "503")
    dns = success(rec, sim, "local-DNS", "net_dns", {"host": "localhost"})
    rec.check("local-DNS-address", dns.get("resolved") is True and any(a in {"127.0.0.1", "::1"} for a in dns.get("addresses", [])))
    tcp = success(rec, sim, "local-TCP-open", "net_tcp", {"host": "127.0.0.1", "port": fake.server.server_port})
    rec.equal("TCP-open", tcp["open"], True)
    rec.note("No external DNS or public HTTP is called. net_* compatibility names are callable even though shell is the advertised discovery surface.")


def framing(rec, sim, fake):
    message = encode({"jsonrpc": "2.0", "id": "chunk", "method": "ping"})
    response = sim.request("POST", "/mcp", message, chunked=True)
    rec.equal("chunked-request", response.json().get("result"), {})
    prefix = b"POST /mcp HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n"
    bad = {
        "duplicate-length": b"Content-Length: 0\r\nContent-Length: 0\r\n\r\n",
        "length-and-transfer": b"Content-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        "negative-length": b"Content-Length: -1\r\n\r\n",
        "noninteger-length": b"Content-Length: nope\r\n\r\n",
        "unsupported-transfer": b"Transfer-Encoding: gzip\r\n\r\n",
        "short-fixed-body": b"Content-Length: 5\r\n\r\n{}",
        "bad-chunk-size": b"Transfer-Encoding: chunked\r\n\r\nZ\r\nx\r\n0\r\n\r\n",
        "missing-chunk-CRLF": b"Transfer-Encoding: chunked\r\n\r\n2\r\n{}xx0\r\n\r\n",
        "unfinished-final-trailer": b"Transfer-Encoding: chunked\r\n\r\n0\r\n",
        "oversized-buffered-declaration": b"Content-Length: 1048577\r\n\r\n",
        "unfinished-header": b"Content-Length: 0\r\n",
        "header-line-ceiling": b"X-Large: " + b"x" * 8192 + b"\r\n\r\n",
        "invalid-header-name": b"Invalid Header: x\r\nContent-Length: 0\r\n\r\n",
        "folded-header": b"X-Test: x\r\n folded\r\nContent-Length: 0\r\n\r\n",
    }
    for case, tail in bad.items():
        response = sim.raw(prefix + tail)
        rec.equal(case, response.status, 400)
    conn = http.client.HTTPConnection("127.0.0.1", sim.port, timeout=bounded_timeout(4))
    try:
        for n in range(2):
            conn.request("GET", "/healthz", headers={"Connection": "keep-alive"})
            response = conn.getresponse()
            rec.check("keepalive-reuse:" + str(n), response.status == 200 and json.loads(response.read())["ok"])
    finally:
        conn.close()
    # Await the interim response before sending any body, rather than faking the handshake.
    with socket.create_connection(("127.0.0.1", sim.port), timeout=bounded_timeout(4)) as conn:
        conn.settimeout(bounded_timeout(4))
        conn.sendall(prefix + b"Expect: 100-continue\r\nContent-Length: " + str(len(message)).encode() + b"\r\n\r\n")
        interim = b""
        while not interim.endswith(b"\r\n\r\n"):
            bounded_timeout(1)
            octet = conn.recv(1)
            if not octet:
                raise EOFError("server closed before Expect response")
            interim += octet
            if len(interim) > 8192:
                raise ValueError("Expect response header exceeded 8 KiB")
        rec.check("expect-continue-before-upload", interim.startswith(b"HTTP/1.1 100 "), short(interim))
        conn.sendall(message)
        response = http.client.HTTPResponse(conn)
        response.begin()
        rec.equal("expect-final-response", json.loads(response.read())["result"], {})
    # Both requests arrive in one send: the buffered body must not eat its successor.
    wire = b"POST /mcp HTTP/1.1\r\nHost: fixture\r\nContent-Length: " + str(len(message)).encode() + b"\r\n\r\n" + message
    wire += b"GET /healthz HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n"
    with socket.create_connection(("127.0.0.1", sim.port), timeout=bounded_timeout(4)) as conn:
        conn.settimeout(bounded_timeout(4))
        conn.sendall(wire)
        reader = conn.makefile("rb")
        try:
            def read_fixed():
                status = int(reader.readline().split()[1])
                heads = {}
                while True:
                    line = reader.readline()
                    if line == b"\r\n":
                        break
                    if not line:
                        raise EOFError("pipelined response header ended early")
                    key, value = line.split(b":", 1)
                    heads[key.lower()] = value.strip()
                return status, json.loads(reader.read(int(heads[b"content-length"])))
            first, second = read_fixed(), read_fixed()
            rec.equal("pipelined-body-first-response", first, (200, {"id": "chunk", "jsonrpc": "2.0", "result": {}}))
            rec.equal("pipelined-successor-response", (second[0], second[1]["ok"]), (200, True))
        finally:
            reader.close()
    rec.note("Strict inbound fixed/chunked framing, EOF, duplicate framing, header/buffer ceilings, pipelining, keepalive and Expect were exercised over actual sockets; packet timing on lwIP/TLS is not inferred.")


def media(rec, sim, fake):
    # This profile declares virtual I2S. It does not prove those pins or parts on hardware.
    for path, mime in [("/audio/pcm", "audio/pcm"), ("/av", "application/x-esp32-av")]:
        response = sim.request("POST", path, b"", {"Content-Type": mime})
        rec.equal("default-profile-empty-stream:" + path, (response.status, response.json()["samples"]), (200, 0))
    rec.check("default-profile-audio-not-advertised", "audio" not in sim.names())
    refusal(rec, sim, "default-profile-audio-refused", "audio", {"command": "speaker_tone", "arguments": {"freq_hz": 440, "ms": 10}})
    sim.post("/api/config", {"board": "esp32s3-xiaozhi-bread"})
    for path, mime in [("/audio/pcm", "audio/pcm"), ("/av", "application/x-esp32-av")]:
        http_case(rec, sim, "method:" + path, "GET", path, status=405)
        http_case(rec, sim, "type:" + path, "POST", path, b"{}", 415)
    value = http_case(rec, sim, "PCM-fixed", "POST", "/audio/pcm", bytes(480), headers={"Content-Type": "audio/pcm"})
    rec.equal("PCM-sample-accounting", (value["bytes"], value["samples"], value["ms"]), (480, 240, 10))
    response = sim.request("POST", "/audio/pcm", bytes(480), {"Content-Type": "audio/pcm"}, chunked=True)
    rec.equal("PCM-chunked-accounting", response.json()["samples"], 240)
    http_case(rec, sim, "PCM-odd-byte", "POST", "/audio/pcm", b"\x00", 400, {"Content-Type": "audio/pcm"})
    http_case(rec, sim, "PCM-rate-refusal", "POST", "/audio/pcm?rate=16000", b"", 400, {"Content-Type": "audio/pcm"})
    http_case(rec, sim, "PCM-30-second-ceiling", "POST", "/audio/pcm", bytes(1440002), 400, {"Content-Type": "audio/pcm"})
    # Above the ordinary buffered 1 MiB limit, but legal for this streaming route.
    value = http_case(rec, sim, "PCM-exact-30-seconds", "POST", "/audio/pcm", bytes(1440000), headers={"Content-Type": "audio/pcm"})
    rec.equal("PCM-exact-ceiling-accounting", (value["bytes"], value["samples"], value["ms"]), (1440000, 720000, 30000))
    record = lambda tag, body: tag.encode() + struct.pack("<H", len(body)) + body
    av = record("F", bytes(512)) + record("A", bytes(480)) + record("E", b"")
    value = http_case(rec, sim, "AV-fixed", "POST", "/av", av, headers={"Content-Type": "application/x-esp32-av"})
    rec.equal("AV-accounting", (value["frames"], value["audio_bytes"], value["samples"], value["ended"]), (1, 480, 240, True))
    response = sim.request("POST", "/av", av, {"Content-Type": "application/octet-stream"}, chunked=True)
    rec.equal("AV-chunked-accounting", (response.status, response.json()["frames"], response.json()["samples"]), (200, 1, 240))
    for case, data in [("unknown-tag", record("X", b"x")), ("frame-size", record("F", b"123")),
                       ("odd-audio-record", record("A", b"x")), ("empty-frame-record", record("F", b"")),
                       ("partial-record-header", b"F\x00"), ("partial-record-payload", b"F\x00\x02x")]:
        response = sim.request("POST", "/av", data, {"Content-Type": "application/x-esp32-av"})
        rec.check("AV-refuses:" + case, response.status >= 400 and response.json().get("ok") is False, short(response.json()))
    prefix = b"POST /audio/pcm HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\nContent-Type: audio/pcm\r\n"
    response = sim.raw(prefix + b"Content-Length: 4\r\n\r\n\x00\x00")
    rec.check("PCM-torn-transport-not-success", response.status == 400 and response.json().get("ok") is False, short(response.json()))
    response = sim.raw(prefix + b"Transfer-Encoding: chunked\r\n\r\n2\r\n\x00\x00xx0\r\n\r\n")
    rec.check("PCM-malformed-chunks-not-success", response.status == 400 and response.json().get("ok") is False, short(response.json()))
    success(rec, sim, "audio-level", "audio", {"command": "mic_level", "arguments": {"ms": 50}})
    wave = success(rec, sim, "audio-record", "audio", {"command": "mic_record", "arguments": {"samples": 64}})
    rec.equal("WAV-header", base64.b64decode(wave["wav_b64"])[:4], b"RIFF")
    success(rec, sim, "audio-tone", "audio", {"command": "speaker_tone", "arguments": {"freq_hz": 440, "ms": 10}})
    # SSE headers are read without waiting for a 120-second body. Own sockets are closed.
    streams = []
    try:
        for n in range(2):
            conn = http.client.HTTPConnection("127.0.0.1", sim.port, timeout=bounded_timeout(4))
            streams.append(conn)
            conn.request("GET", "/api/agent/stream?after=0", headers={"Accept": "text/event-stream"})
            response = conn.getresponse()
            rec.check("SSE-open:" + str(n), response.status == 200 and response.getheader("Content-Type") == "text/event-stream"
                      and response.getheader("Transfer-Encoding") == "chunked")
        refused = sim.request("GET", "/api/agent/stream")
        rec.equal("SSE-third-stream-refused", (refused.status, refused.headers.get("retry-after")), (503, "5"))
        rec.equal("SSE-does-not-hold-app-lock", sim.request("GET", "/api/status", timeout=2).status, 200)
    finally:
        for conn in streams:
            conn.close()
    rec.note("PCM/AV record and HTTP framing, accounting, limits, audio tools and two SSE slots use virtual speaker/microphone/display. Analog audio, real camera, electrically correct pins, SSE heartbeat/120 s lifetime are unverified.")


def fnv_key(path):
    value = 0xcbf29ce484222325
    for byte in path.encode():
        value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    return "ws.%012x" % (value & 0xffffffffffff)


def storage(rec, sim, fake):
    # Adversarial state fixtures are written only after stopping our own simulator.
    for case, path, text in [("loader-traversal", "/../escape", "bad path"),
                              ("loader-file-size", "/oversized.txt", "x" * 262145)]:
        sim.stop()
        initial = {"config": encode({"skills": {"seed_defaults": False}}).decode(),
                   "ws.index": encode({path: fnv_key(path)}).decode(), fnv_key(path): text}
        sim.state.write_bytes(encode(initial))
        sim.start()
        tree = sim.get("/api/files/tree")
        rec.check(case + ".refused-at-boot", not any(e["path"] == path for e in tree["entries"]), short(tree))
        rec.equal(case + ".server-survives", sim.request("GET", "/healthz").status, 200)
    sim.stop()
    sim.state.write_bytes(encode({"ws.index": "not valid JSON"}))
    sim.start()
    rec.equal("corrupt-index-survives", sim.request("GET", "/api/files/tree").status, 200)
    sim.drain()
    # A directory at the destination makes rename fail on Windows and Unix without ACL tricks.
    sim.state.unlink()
    sim.state.mkdir()
    saved = sim.post("/api/agent/config", {"model": "cannot-persist"})
    rec.check("failed-save-not-acknowledged-as-durable", saved.get("saved") is not True, short(saved))
    rec.equal("config-response-distinguishes-admission", saved.get("queued"), True)
    result = sim.call("write", {"path": "/cannot-save.txt", "text": "must not claim durable"})
    rec.check("failed-workspace-save-not-acknowledged-as-durable", result.get("isError") is True or content(result).get("ok") is not True, short(result))
    status = wait_for(lambda: sim.get("/api/status")["store"], lambda s: s["pending"] == 0 and s["failed"] > 0)
    rec.check("persistence-failure-observable", status["last_failure"] is not None and bool(status["refused_keys"]), short(status))
    rec.note("Private malformed workspace index, path/file budgets at load and deterministic destination-rename failure are tested. Real NVS/flash full/power-loss atomicity requires its own hardware evidence.")


AREAS = {"discovery": discovery, "rpc": rpc, "tools": tools, "permissions": permissions, "files": files,
         "manifest": manifest, "scripts": scripts, "sched": sched, "agent": agent, "transport": transport,
         "network": network, "framing": framing, "media": media, "storage": storage}


def main():
    global RUN_DEADLINE
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--sim", type=Path, default=ROOT / "target" / "debug" / ("esp32-agent-sim.exe" if os.name == "nt" else "esp32-agent-sim"))
    parser.add_argument("--areas", default="*", help="comma-separated area names/globs; each selected area gets independent state")
    parser.add_argument("--list", action="store_true", help="list selectable areas, then exit")
    parser.add_argument("--verbose", action="store_true", help="also print passing assertion names")
    parser.add_argument("--report", type=Path, help="write all checks/coverage as UTF-8 JSON (e.g. target/interfaces.json)")
    parser.add_argument("--deadline", type=float, default=240, help="overall run budget in seconds, plus <=8 s cleanup (default 240)")
    args = parser.parse_args()
    if args.list:
        print("\n".join(AREAS))
        return 0
    selected = [a for a in AREAS if any(fnmatch.fnmatchcase(a, p.strip()) for p in args.areas.split(","))]
    if not selected:
        parser.error("no matching areas; use --list")
    binary = args.sim.resolve()
    if not binary.is_file():
        parser.error("simulator missing; run cargo build -p esp32-agent-sim or supply --sim")
    if not math.isfinite(args.deadline) or args.deadline < 1:
        parser.error("--deadline must be finite and at least 1 second")
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    rec = Recorder(args.verbose)
    started = time.monotonic()
    try:
        commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, timeout=5).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        commit = "unknown"
    RUN_DEADLINE = started + args.deadline
    deadline_fired = False
    children = []
    try:
        with tempfile.TemporaryDirectory(prefix="esp32-interface-audit-") as directory, FixtureServer() as fake:
            for area in selected:
                rec.area = area
                before = len(rec.checks)
                start = time.monotonic()
                if deadline_fired or time.monotonic() >= RUN_DEADLINE:
                    deadline_fired = True
                    rec.check("not-run-deadline", False, "overall budget exceeded before this area")
                    rec.area_summary[area] = {"passed": 0, "failed": 1, "checks": 1, "elapsed_s": 0, "not_run": True}
                    continue
                sim = Simulator(binary, Path(directory) / area, rec)
                children.append(sim)
                try:
                    with sim:
                        AREAS[area](rec, sim, fake)
                except Exception as error:
                    deadline_fired |= isinstance(error, DeadlineExpired)
                    rec.check("area-completed", False, type(error).__name__ + ": " + str(error))
                    rec.note(traceback.format_exc(limit=5))
                finally:
                    sim.stop()
                    children.remove(sim)
                checks = rec.checks[before:]
                failures = sum(not c["passed"] for c in checks)
                elapsed = time.monotonic() - start
                rec.area_summary[area] = {"passed": len(checks) - failures, "failed": failures,
                                          "checks": len(checks), "elapsed_s": round(elapsed, 3)}
                print("AREA %-12s %3d passed / %3d failed / %3d checks (%.2fs)" %
                      (area, len(checks) - failures, failures, len(checks), elapsed), flush=True)
    except KeyboardInterrupt:
        rec.check("interrupted", False, "operator interrupted this run")
    finally:
        for sim in children:
            sim.stop()
        RUN_DEADLINE = None
    passed = sum(c["passed"] for c in rec.checks)
    failed = len(rec.checks) - passed
    report = {"schema": 1, "commit": commit, "simulator": str(binary), "simulator_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "python": sys.version, "platform": sys.platform,
              "selected_areas": selected, "elapsed_s": round(time.monotonic() - started, 3),
              "passed": passed, "failed": failed, "checks": len(rec.checks), "assertions": rec.checks,
              "area_summary": rec.area_summary, "coverage": rec.coverage(), "tool_inventory": rec.inventory,
              "unverified": ["firmware not built/verified", "real boards/electrical behavior",
              "real providers/TLS/credits", "unmodeled hardware drivers", "SSE lifetime and real-time radio/power-loss behavior"]}
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_bytes(encode(report) + b"\n")
        print("REPORT " + str(args.report.resolve()), flush=True)
    print("TOTAL %d passed / %d failed / %d checks (%.2fs)" % (passed, failed, len(rec.checks), report["elapsed_s"]), flush=True)
    return 1 if failed or deadline_fired else 0


if __name__ == "__main__":
    raise SystemExit(main())
