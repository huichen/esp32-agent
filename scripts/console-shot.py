#!/usr/bin/env python3
"""Screenshot the board's web console.

The portal is the only surface a person actually looks at, so the README and a bug
report both need pictures of it, and taking them by hand is where they go stale.
This drives a headless Chrome/Chromium: the same browser the page tests measure
with, found the same way (`ESP32_AGENT_BROWSER`, then the usual install paths).

It is deliberately tiny — no CDP, no driver, no dependency. Chrome's own
`--screenshot` writes the viewport, which is why the size is a parameter: the chat
is taller than a phone, so pass a tall window when you want more of it.

    python scripts/console-shot.py --url http://192.168.3.7 --out shot.png
    python scripts/console-shot.py --url http://192.168.3.7/?panel=devices \\
        --out panel.png --size 900x1100

What it will not do: click, scroll to an element, or wait for a selector. Panels
are reachable by deep link (`?panel=devices`) because of that, not by scripting a
click.
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile


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
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ]
    else:
        candidates += [shutil.which(name) for name in
                       ("google-chrome", "google-chrome-stable", "chromium", "chromium-browser")]
    for path in candidates:
        if path and os.path.exists(path):
            return path
    raise SystemExit(
        "no Chrome/Chromium found; install one or point ESP32_AGENT_BROWSER at its executable"
    )


def parse_size(text):
    try:
        width, height = text.lower().split("x", 1)
        return int(width), int(height)
    except ValueError:
        raise SystemExit(f"--size must look like 390x844, not {text!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", required=True, help="page to capture, e.g. http://192.168.3.7")
    parser.add_argument("--out", required=True, help="PNG to write")
    parser.add_argument("--size", default="390x844", help="viewport, WIDTHxHEIGHT (default 390x844)")
    parser.add_argument("--wait", type=int, default=4000,
                        help="milliseconds of page time before the shot (default 4000); the "
                             "console fetches its history, so too short a wait captures an "
                             "empty conversation")
    parser.add_argument("--scale", type=float, default=2.0,
                        help="device scale factor (default 2: a retina-sized PNG reads well "
                             "in a README)")
    args = parser.parse_args()

    width, height = parse_size(args.size)
    out = os.path.abspath(args.out)
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    # A throwaway profile: the user's own Chrome profile must not be touched, and a
    # first-run dialog in a screenshot is not a picture of the board.
    with tempfile.TemporaryDirectory(prefix="console-shot-") as profile:
        command = [
            browser(),
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            f"--user-data-dir={profile}",
            "--hide-scrollbars",
            f"--window-size={width},{height}",
            f"--force-device-scale-factor={args.scale}",
            f"--virtual-time-budget={args.wait}",
            f"--screenshot={out}",
            args.url,
        ]
        result = subprocess.run(command, capture_output=True, text=True, timeout=120)
    if not os.path.exists(out):
        sys.stderr.write(result.stderr or result.stdout or "chrome wrote no file\n")
        raise SystemExit("the screenshot was not written")
    size = os.path.getsize(out)
    print(f"{out}  {width}x{height} @{args.scale}x  {size} bytes")


if __name__ == "__main__":
    main()
