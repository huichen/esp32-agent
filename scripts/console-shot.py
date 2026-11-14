#!/usr/bin/env python3
"""Screenshot the board's web console at an exact viewport size.

Chrome's `--window-size` has a floor (measured on this machine: asking for 390 px gives a
534 px layout) and `--screenshot` then captures only the requested width — so every "phone"
capture came back with the right-hand 144 px missing. The page was fine; the picture was
cropped, which is worse, because it looks like a layout bug.

This wraps the page in an iframe of exactly the requested size (an iframe *is* a viewport),
captures at a window size the browser will accept, and crops to the frame. Chrome and the
crop are the only dependencies; Pillow is used when present, and its absence is reported
rather than guessed at.

    python scripts/console-shot.py --url http://192.168.3.7 --out chat.png
    python scripts/console-shot.py --url 'http://192.168.3.7/?panel=devices' --out panel.png \\
        --size 900x1200

`--panel` is a shortcut for the deep link, which exists because a headless browser cannot
click a tab.
"""
import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HARNESS = """<!doctype html>
<html><head><meta charset="utf-8"><title>shot</title>
<style>html,body{margin:0;padding:0;background:#000;overflow:hidden}
iframe{border:0;display:block}</style></head>
<body><iframe src="TARGET" width="WIDTH" height="HEIGHT"></iframe></body></html>
"""


class Handler(BaseHTTPRequestHandler):
    page = b""

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(self.page)))
        self.end_headers()
        self.wfile.write(self.page)


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
        candidates += [shutil.which(n) for n in ("google-chrome", "google-chrome-stable", "chromium")]
    for path in candidates:
        if path and os.path.exists(path):
            return path
    raise SystemExit("no Chrome/Chromium found; set ESP32_AGENT_BROWSER")


def parse_size(text):
    try:
        w, h = text.lower().split("x", 1)
        return int(w), int(h)
    except ValueError:
        raise SystemExit(f"--size must look like 390x844, not {text!r}")


def png_size(path):
    with open(path, "rb") as f:
        head = f.read(33)
    return struct.unpack(">II", head[16:24])


def crop(path, width, height):
    try:
        from PIL import Image
    except ImportError:
        print("Pillow is not installed, so the image was not cropped; its size is "
              f"{png_size(path)} and the page occupies the top-left {width}x{height}",
              file=sys.stderr)
        return False
    with Image.open(path) as image:
        image.crop((0, 0, min(width, image.width), min(height, image.height))).save(path)
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", required=True, help="page to capture, e.g. http://192.168.3.7")
    parser.add_argument("--out", required=True, help="PNG to write")
    parser.add_argument("--size", default="390x844", help="viewport, WIDTHxHEIGHT (default 390x844)")
    parser.add_argument("--wait", type=int, default=4000,
                        help="milliseconds of page time before the shot (default 4000); the "
                             "console fetches its history, so too short a wait captures nothing")
    parser.add_argument("--scale", type=float, default=2.0,
                        help="device scale factor (default 2, so the PNG is readable in a README)")
    parser.add_argument("--panel", help="shortcut for ?panel=<name> (devices, tools, agent, ...)")
    args = parser.parse_args()

    width, height = parse_size(args.size)
    url = args.url
    if args.panel:
        url += ("&" if "?" in url else "?") + "panel=" + args.panel

    Handler.page = (HARNESS.replace("TARGET", url)
                    .replace("WIDTH", str(width)).replace("HEIGHT", str(height))).encode()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()

    out = os.path.abspath(args.out)
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    # The window must be wider than the browser's floor, or the harness itself is cropped
    # before the frame is even considered. 700 is comfortably above the 534 measured here.
    window_w = max(700, width)
    window_h = max(600, height)
    with tempfile.TemporaryDirectory(prefix="console-shot-") as profile:
        result = subprocess.run(
            [browser(), "--headless=new", "--disable-gpu", "--hide-scrollbars",
             "--no-first-run", "--no-default-browser-check",
             f"--user-data-dir={profile}",
             f"--window-size={window_w},{window_h}",
             f"--force-device-scale-factor={args.scale}",
             f"--virtual-time-budget={args.wait}",
             f"--screenshot={out}",
             f"http://127.0.0.1:{port}/"],
            capture_output=True, timeout=180,
        )
    if not os.path.exists(out):
        sys.stderr.write(result.stderr.decode("utf-8", "replace")[-800:])
        raise SystemExit("the screenshot was not written")

    want = (round(width * args.scale), round(height * args.scale))
    had = png_size(out)
    cropped = crop(out, want[0], want[1])
    print(f"{out}  {width}x{height} @{args.scale}x -> {want[0]}x{want[1]}")
    if had != want:
        print(f"  chrome produced {had[0]}x{had[1]}; "
              f"{'cropped' if cropped else 'NOT cropped'} to the frame", file=sys.stderr)


if __name__ == "__main__":
    main()
