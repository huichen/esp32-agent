#!/usr/bin/env python3
"""Screenshot the board's web console at an exact viewport size.

Two things this has to get right, both learned the hard way:

* **The viewport.** Chrome will not make a window narrower than about 534 px whatever
  `--window-size` says, while `--screenshot` captures the width it was asked for — so a
  "390 px" capture was the left 390 px of a 534 px layout, with everything on the right
  missing. An iframe *is* a viewport, so the page is framed at exactly the requested size
  and the PNG is cropped to it.
* **Same origin.** The portal keeps its state in `localStorage` and follows its journal over
  `EventSource`. In a third-party iframe Chrome blocks the first and the page never leaves
  "connecting…". So the harness and the page are served through one local proxy, which makes
  the frame same-origin with the harness and the API calls same-origin with the page.

    python scripts/console-shot.py --url http://192.168.3.7 --out chat.png
    python scripts/console-shot.py --url http://192.168.3.7 --out devices.png \\
        --size 390x900 --panel devices
"""
import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HARNESS = """<!doctype html>
<html><head><meta charset="utf-8"><title>shot</title>
<style>html,body{margin:0;padding:0;background:#0b0f14;overflow:hidden}
iframe{border:0;display:block}</style></head>
<body><iframe src="FRAME_SRC" width="WIDTH" height="HEIGHT"></iframe></body></html>
"""


class Proxy(BaseHTTPRequestHandler):
    board = ""
    harness = b""

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.forward("GET", None)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        self.forward("POST", self.rfile.read(length) if length else None)

    def forward(self, method, body):
        if self.path == "/":
            self.reply(200, "text/html; charset=utf-8", self.harness)
            return
        # `/page` is the board's own console, served from this origin so that its
        # localStorage and EventSource are first-party.
        path = "/" + self.path[6:] if self.path.startswith("/page") else self.path
        url = self.board.rstrip("/") + path
        req = urllib.request.Request(url, data=body, method=method)
        for header in ("Content-Type", "Accept", "Authorization", "X-Portal-Pin"):
            value = self.headers.get(header)
            if value:
                req.add_header(header, value)
        try:
            with urllib.request.urlopen(req, timeout=30) as response:
                data = response.read()
                status = response.status
                ctype = response.headers.get("Content-Type", "application/octet-stream")
        except urllib.error.HTTPError as exc:
            data, status, ctype = exc.read(), exc.code, exc.headers.get("Content-Type", "text/plain")
        except Exception as exc:
            data, status, ctype = str(exc).encode(), 502, "text/plain"
        # The SSE stream never ends; buffering it would hang the request.
        self.reply(status, ctype, data)

    def reply(self, status, ctype, data):
        try:
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass


def browser():
    override = os.environ.get("ESP32_AGENT_BROWSER")
    if override:
        return override
    candidates = []
    if os.name == "nt":
        for base in ("ProgramFiles", "ProgramFiles(x86)", "LocalAppData"):
            root = os.environ.get(base)
            if not root:
                continue
            candidates.append(os.path.join(root, "Google", "Chrome", "Application", "chrome.exe"))
            candidates.append(os.path.join(root, "Microsoft", "Edge", "Application", "msedge.exe"))
    elif sys.platform == "darwin":
        candidates += [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    else:
        candidates += [shutil.which(n) for n in ("google-chrome", "chrome", "chromium")]
    for path in candidates:
        if path and os.path.exists(path):
            return path
    raise SystemExit("no Chrome/Chromium found; set ESP32_AGENT_BROWSER")


def png_size(path):
    with open(path, "rb") as handle:
        head = handle.read(33)
    return struct.unpack(">II", head[16:24])


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", required=True, help="board base URL, e.g. http://192.168.3.7")
    parser.add_argument("--out", required=True, help="PNG to write")
    parser.add_argument("--size", default="390x844", help="viewport, WIDTHxHEIGHT")
    parser.add_argument("--panel", help="open a panel: devices, tools, agent, files, terminal, settings")
    parser.add_argument("--wait", type=int, default=8000,
                        help="milliseconds of page time before the shot (default 8000); the "
                             "console fetches its history and opens its journal stream")
    parser.add_argument("--scale", type=float, default=2.0, help="device scale factor")
    args = parser.parse_args()

    try:
        width, height = (int(part) for part in args.size.lower().split("x", 1))
    except ValueError:
        raise SystemExit(f"--size must look like 390x844, not {args.size!r}")

    frame = "/page"
    if args.panel:
        frame += "?panel=" + urllib.parse.quote(args.panel)
    Proxy.board = args.url
    Proxy.harness = (HARNESS.replace("FRAME_SRC", frame)
                     .replace("WIDTH", str(width)).replace("HEIGHT", str(height))).encode()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    server.daemon_threads = True
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()

    out = os.path.abspath(args.out)
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    # Wider than the browser's floor, so the harness itself is never the thing cropped.
    with tempfile.TemporaryDirectory(prefix="console-shot-") as profile:
        result = subprocess.run(
            [browser(), "--headless=new", "--disable-gpu", "--hide-scrollbars",
             "--no-first-run", "--no-default-browser-check",
             f"--user-data-dir={profile}",
             f"--window-size={max(700, width)},{max(600, height)}",
             f"--force-device-scale-factor={args.scale}",
             f"--virtual-time-budget={args.wait}",
             f"--screenshot={out}",
             f"http://127.0.0.1:{port}/"],
            capture_output=True, timeout=240,
        )
    if not os.path.exists(out):
        sys.stderr.write(result.stderr.decode("utf-8", "replace")[-800:])
        raise SystemExit("the screenshot was not written")

    want = (round(width * args.scale), round(height * args.scale))
    had = png_size(out)
    try:
        from PIL import Image
    except ImportError:
        print(f"{out}  {had[0]}x{had[1]} (Pillow missing, not cropped to {want[0]}x{want[1]})")
        return
    with Image.open(out) as image:
        image.crop((0, 0, min(want[0], image.width), min(want[1], image.height))).save(out)
    print(f"{out}  {width}x{height} @{args.scale}x -> {want[0]}x{want[1]}")


if __name__ == "__main__":
    main()
