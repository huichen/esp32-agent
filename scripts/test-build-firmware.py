#!/usr/bin/env python3
"""Offline regressions for both firmware build wrappers; never build or flash a board.

Python stdlib only. Copies the wrappers/config into a temporary fixture and mocks
cargo, git, rustup, ldproxy and espflash. Run from any directory; --shell selects
one wrapper, and --scripts-dir permits regression-testing another checkout.
"""
from __future__ import annotations

import argparse
import base64
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent
SCRIPTS = REPO / "scripts"
SHELLS: dict[str, str] = {}
TARGET = "xtensa-esp32s3-espidf"
CONFIG = '''[build]
target = "xtensa-esp32s3-espidf"
[target.xtensa-esp32s3-espidf]
linker = "ldproxy"
runner = "espflash flash --monitor"
[env]
MCU = "esp32s3"
'''
MOCK = r'''
import json, os, pathlib, sys

def native(value):
    if os.name == "nt" and len(value) > 2 and value[0] == "/" and value[2] == "/":
        value = value[1].upper() + ":" + value[2:]
    return pathlib.Path(value)

name, args = sys.argv[1], sys.argv[2:]
with open(os.environ["MOCK_LOG"], "a", encoding="utf-8") as out:
    out.write(json.dumps({"name": name, "args": args,
        "target_dir": os.environ.get("CARGO_TARGET_DIR", ""),
        "commit": os.environ.get("ESP32_AGENT_COMMIT", ""),
        "built_at": os.environ.get("ESP32_AGENT_BUILT_AT", ""),
        "clang": os.environ.get("LIBCLANG_PATH", "")}) + "\n")
if name == "git":
    if len(args) < 2 or args[0] != "-C" or native(args[1]) != native(os.environ["MOCK_REPO"]):
        sys.exit(2)
    print("abc1234")
elif name == "rustup":
    print(os.environ.get("MOCK_TOOLCHAIN", "esp (active)"))
elif name == "cargo":
    if "config" in args:
        sys.exit(1)  # unstable probe failing is expected, not a failed build
    if os.environ.get("MOCK_BUILD_FAIL"):
        sys.exit(7)
    if not os.environ.get("MOCK_NO_ELF"):
        target = args[args.index("--target") + 1]
        profile = "release" if "--release" in args else "debug"
        elf = native(os.environ["CARGO_TARGET_DIR"]) / target / profile / "esp32-agent"
        elf.parent.mkdir(parents=True, exist_ok=True)
        elf.write_text("new " + profile, encoding="utf-8")
elif name == "espflash" and os.environ.get("MOCK_FLASH_FAIL"):
    sys.exit(9)
'''


def native(value: str) -> Path:
    if os.name == "nt" and re.match(r"^/[a-zA-Z]/", value):
        value = value[1].upper() + ":" + value[2:]
    return Path(value)


def ps_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


class WrapperTests(unittest.TestCase):
    shell: str

    @contextmanager
    def fixture(self):
        with tempfile.TemporaryDirectory(prefix="firmware-wrapper-") as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            (root / "firmware" / ".cargo").mkdir(parents=True)
            (root / "firmware" / ".cargo" / "config.toml").write_text(CONFIG, encoding="utf-8")
            table = root / "firmware" / "partitions.csv"
            table.write_text("factory,app,factory,0x10000,0x400000\n", encoding="utf-8")
            script = root / "scripts" / ("build-firmware.sh" if self.shell == "bash" else "build-firmware.ps1")
            shutil.copyfile(SCRIPTS / script.name, script)
            mock = root / "mock.py"
            mock.write_text(MOCK, encoding="utf-8")
            home = root / "home"
            home.mkdir()
            log = root / "calls.jsonl"
            output = root / "output with spaces"
            env = os.environ.copy()
            for key in ("CARGO_TARGET_DIR", "ESP32_AGENT_COMMIT", "ESP32_AGENT_BUILT_AT"):
                env.pop(key, None)
            env.update(MOCK_LOG=str(log), MOCK_REPO=str(root), HOME=str(home), USERPROFILE=str(home))
            if self.shell == "bash":
                bin_dir = root / "bin"
                bin_dir.mkdir()
                for name in ("cargo", "rustup", "git", "ldproxy", "espflash"):
                    stub = bin_dir / name
                    stub.write_text("#!/usr/bin/env bash\nexec " +
                                    json.dumps(sys.executable.replace("\\", "/")) + " " +
                                    json.dumps(str(mock).replace("\\", "/")) + " " + name + ' "$@"\n',
                                    encoding="utf-8", newline="\n")
                    stub.chmod(0o755)
                env["PATH"] = str(bin_dir) + os.pathsep + env["PATH"]
                # Git for Windows prepends its own git.exe directory when it
                # imports a Windows PATH. Prepend the mock again inside Bash.
                posix_bin = str(bin_dir).replace("\\", "/")
                if os.name == "nt" and re.match(r"^[A-Za-z]:/", posix_bin):
                    posix_bin = "/" + posix_bin[0].lower() + posix_bin[2:]
                env["MOCK_BIN"] = posix_bin

            def run(*flags: str, **updates: str):
                current = dict(env, **updates)
                if self.shell == "bash":
                    command = [SHELLS[self.shell], "--noprofile", "--norc", "-c",
                               'export PATH="$MOCK_BIN:$PATH"; exec bash "$0" "$@"', str(script), *flags]
                else:
                    functions = "\n".join(
                        "function global:" + name + " { & " + ps_quote(sys.executable) + " " +
                        ps_quote(str(mock)) + " " + name +
                        " @args; $global:LASTEXITCODE = $LASTEXITCODE }"
                        for name in ("cargo", "rustup", "git", "ldproxy", "espflash"))
                    # Execute switches inside PowerShell: -File cannot reliably pass
                    # -Release:$false on Windows PowerShell 5.1.
                    switches = []
                    for flag in flags:
                        if flag.startswith("-"):
                            switches.append(flag)
                        else:
                            switches.append(ps_quote(flag))
                    code = functions + "\n& " + ps_quote(str(script)) + " " + " ".join(switches)
                    encoded = base64.b64encode(code.encode("utf-16le")).decode("ascii")
                    command = [SHELLS[self.shell], "-NoProfile", "-NonInteractive", "-EncodedCommand", encoded]
                completed = subprocess.run(command, cwd=root, env=current, capture_output=True,
                                           text=True, encoding="utf-8", errors="replace", timeout=30)
                calls = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()] if log.exists() else []
                return completed, calls

            yield root, output, table, run

    def flags(self, flash=False, debug=False, target_dir=None, port=None):
        flags = []
        if flash:
            flags.append("--flash" if self.shell == "bash" else "-Flash")
        if debug:
            flags.append("--debug" if self.shell == "bash" else "-Release:$false")
        if target_dir is not None:
            flags.extend(("--target-dir" if self.shell == "bash" else "-TargetDir", str(target_dir)))
        if port is not None:
            flags.extend(("--port" if self.shell == "bash" else "-Port", port))
        return flags

    def okay(self, completed):
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)

    def build(self, calls):
        return next(call for call in calls if call["name"] == "cargo" and "build" in call["args"])

    def test_release_metadata_and_unchanged_config(self):
        with self.fixture() as (root, _, _, run):
            done, calls = run()
            self.okay(done)
            built = self.build(calls)
            self.assertIn("--release", built["args"])
            self.assertEqual(built["commit"], "abc1234", json.dumps(calls, ensure_ascii=False))
            self.assertRegex(built["built_at"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
            self.assertEqual(native(built["target_dir"]), root / "target")
            self.assertFalse(any(call["name"] == "espflash" for call in calls))
            self.assertEqual((root / "firmware" / ".cargo" / "config.toml").read_text(encoding="utf-8"), CONFIG)

    def test_explicit_output_overrides_inherited_environment(self):
        with self.fixture() as (root, output, _, run):
            done, calls = run(*self.flags(target_dir=output), CARGO_TARGET_DIR=str(root / "stale"))
            self.okay(done)
            self.assertEqual(native(self.build(calls)["target_dir"]), output)

    def test_inherited_output_is_honored_without_flag(self):
        with self.fixture() as (_, output, _, run):
            done, calls = run(CARGO_TARGET_DIR=str(output))
            self.okay(done)
            self.assertEqual(native(self.build(calls)["target_dir"]), output)

    def test_debug_flashes_exact_elf_and_explicit_partition(self):
        with self.fixture() as (_, output, table, run):
            stale = output / TARGET / "release" / "esp32-agent"
            stale.parent.mkdir(parents=True)
            stale.write_text("old release", encoding="utf-8")
            done, calls = run(*self.flags(flash=True, debug=True, target_dir=output, port="COM stub"))
            self.okay(done)
            self.assertNotIn("--release", self.build(calls)["args"])
            args = next(call["args"] for call in calls if call["name"] == "espflash")
            self.assertEqual(native(args[2]), output / TARGET / "debug" / "esp32-agent")
            self.assertEqual(native(args[args.index("--partition-table") + 1]), table)
            self.assertEqual(args[args.index("--target-app-partition") + 1], "factory")
            self.assertEqual(args[args.index("--port") + 1], "COM stub")
            self.assertEqual(stale.read_text(encoding="utf-8"), "old release")

    def test_missing_selected_elf_does_not_flash_other_profile(self):
        with self.fixture() as (_, output, _, run):
            stale = output / TARGET / "debug" / "esp32-agent"
            stale.parent.mkdir(parents=True)
            stale.write_text("old debug", encoding="utf-8")
            done, calls = run(*self.flags(flash=True, target_dir=output), MOCK_NO_ELF="1")
            self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertFalse(any(call["name"] == "espflash" for call in calls))

    def test_build_failure_never_flashes(self):
        with self.fixture() as (_, output, _, run):
            done, calls = run(*self.flags(flash=True, target_dir=output), MOCK_BUILD_FAIL="1")
            self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertFalse(any(call["name"] == "espflash" for call in calls))

    def test_flash_failure_is_not_success(self):
        with self.fixture() as (_, output, _, run):
            done, calls = run(*self.flags(flash=True, target_dir=output), MOCK_FLASH_FAIL="1")
            self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertEqual(sum(call["name"] == "espflash" for call in calls), 1)

    def test_missing_partition_table_never_flashes(self):
        with self.fixture() as (_, output, table, run):
            table.unlink()
            done, calls = run(*self.flags(flash=True, target_dir=output))
            self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertFalse(any(call["name"] == "espflash" for call in calls))

    def test_wrong_toolchain_is_refused(self):
        with self.fixture() as (_, _, _, run):
            done, calls = run(MOCK_TOOLCHAIN="noesp (default)")
            self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertFalse(any(call["name"] == "cargo" and "build" in call["args"] for call in calls))

    def test_missing_option_value_is_refused(self):
        with self.fixture() as (_, _, _, run):
            flag = "--target-dir" if self.shell == "bash" else "-TargetDir"
            done, calls = run(flag)
            self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertFalse(any(call["name"] == "cargo" and "build" in call["args"] for call in calls))

    def test_clang_library_is_converted_to_its_directory(self):
        with self.fixture() as (root, _, _, run):
            library = root / "libclang.dll"
            library.write_bytes(b"fixture")
            done, calls = run(LIBCLANG_PATH=str(library))
            self.okay(done)
            self.assertEqual(native(self.build(calls)["clang"]), root)


def main():
    global SCRIPTS
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shell", choices=("all", "bash", "powershell"), default="all")
    parser.add_argument("--scripts-dir", type=Path, default=SCRIPTS)
    parser.add_argument("--case", help="run test methods matching this substring")
    args = parser.parse_args()
    SCRIPTS = args.scripts_dir.resolve()
    chosen = ("bash", "powershell") if args.shell == "all" else (args.shell,)
    suite = unittest.TestSuite()
    for shell in chosen:
        program = shutil.which(shell) or (shutil.which("pwsh") if shell == "powershell" else None)
        if not program:
            if args.shell != "all":
                parser.error(f"{shell} is not installed")
            print(f"SKIP {shell}: not installed", file=sys.stderr)
            continue
        SHELLS[shell] = program
        case = type(shell.title() + "Tests", (WrapperTests,), {"shell": shell})
        loader = unittest.TestLoader()
        if args.case:
            loader.testNamePatterns = ["*" + args.case + "*"]
        suite.addTests(loader.loadTestsFromTestCase(case))
    if not suite.countTestCases():
        parser.error("no shell available for tests")
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
