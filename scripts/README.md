# `scripts/` — what a person or CI runs

These are development-machine checks and build wrappers. The checks need no board;
`build-firmware` also supports an explicit **flash** option, which does write an attached
board. Bench/media utilities live in [`tools/`](../tools/).

| Script | What it does | Needs |
|---|---|---|
| `smoke.ps1` / `smoke.sh` | end-to-end gate against the host simulator: portal, `tools/list`, a live manifest change, an out-of-range call that must be refused | `cargo`; Bash wrapper also uses `curl`, `python3` |
| `build-firmware.ps1` / `build-firmware.sh` | build the ESP-IDF firmware, optionally flash it | `espup` + ESP-IDF, see [flashing](../docs/flashing.md) |
| `test-interfaces.py` | live local HTTP/MCP and fake-model agent regressions, with isolated simulator state | built `esp32-agent-sim`, Python 3 standard library |
| `test-build-firmware.py` | offline mocked regressions for both firmware wrappers; does not build or flash | Python 3 standard library; Bash/PowerShell for their respective cases |
| `check-readmes.py` | offline structural, numeric and local-link check for the eight README languages | Python 3.10+, standard library only |

Build and smoke wrappers exist twice, once in PowerShell and once in Bash, and the two run
the same commands in the same order — `smoke.sh` is `smoke.ps1` line for line, and
`build-firmware.sh` is `build-firmware.ps1` with flags instead of parameters. A change
to one is a change to both.

```bash
bash scripts/smoke.sh                          # the gate
bash scripts/build-firmware.sh                 # build
bash scripts/build-firmware.sh --flash --port /dev/ttyACM0
```

```powershell
powershell -File scripts/smoke.ps1
powershell -File scripts/build-firmware.ps1 -Flash -Port COM3
```

`--debug` (`-Release:$false`) builds the debug profile; `--target-dir` (`-TargetDir`)
moves the artifacts when the clone path is too long for `esp-idf-sys`. An explicit output
directory overrides an inherited `CARGO_TARGET_DIR`. Flashing selects only the ELF from
the requested profile and passes `firmware/partitions.csv` and the `factory` app slot;
a missing ELF/table or failed build/flash is an error, never a fallback to an older image.

## Simulator interface regressions

```text
cargo build -p esp32-agent-sim
python scripts/test-interfaces.py --report target/interfaces.json
python scripts/test-interfaces.py --list
python scripts/test-interfaces.py --areas rpc,permissions,transport
```

The suite uses Python's standard library, temporary isolated state and random loopback
ports. Each selected area starts its own simulator; local fake Chat/Responses/Anthropic
and image endpoints exercise agent adapters without provider credentials. The 14 areas
cover discovery, RPC, tools/permissions, files/formats, scripts, agent protocols/lifecycle,
scheduling, media and transport. A failed assertion makes the process exit nonzero; the
optional UTF-8 JSON report records assertions, exercised routes/methods/tools and gaps.

Use `--sim <binary>` for a non-default build location, `--verbose` for passing case names,
and `--deadline <seconds>` to change the overall run budget (default 240 seconds, plus
bounded cleanup). The suite does not use the operator's `sim-state`, provider accounts,
credentials or hardware. It does **not** verify ESP-IDF, board performance/power loss,
real-provider compatibility, or Unix runtime merely because it runs on Windows. Results
must be recorded with the tested commit; this page does not claim an intermediate run is
final audit coverage.

## Offline build-wrapper regression checks

```text
python scripts/test-build-firmware.py
```

This uses temporary fixtures and mocked commands: **no compilation, serial access or
flashing**. There are 22 cases when both wrappers' shells are installed. It checks both
installed shells (or `--shell bash` / `--shell powershell`),
including metadata, output-directory precedence, debug/release selection, explicit
partition arguments, and failure propagation. Missing shells are reported as skipped.
It does not validate the ESP-IDF toolchain or the firmware image itself.

## README translation check

After changing a README, check all eight languages against the English source:

```bash
python3 scripts/check-readmes.py
python3 scripts/check-readmes.py --self-test
```

On Windows, use `python` instead of `python3`. The repository root defaults to the
script's parent repository, not the current directory; `--root <path>` checks a
specific checkout.

The check requires the eight-language switcher on line 3 (only the current language
bold, seven relative links), equal level-2 heading counts, identical fenced code
blocks, and matching table row/column structure and alignment. It compares numeric
literals per level-2 section, including inline configuration examples and digits
in versions/identifiers; decimal commas such as `13,7` are normalized to `13.7`.
It resolves local Markdown links and images, including reference links and
percent-encoded fragments, against
GitHub-style Unicode heading slugs with duplicate suffixes. HTML anchor hacks are
rejected. Relative paths must exist; heading fragments must target Markdown files
(or a directory's `README.md`). Remote and website-root links are not fetched.

`--self-test` additionally checks valid in-memory fixtures and 14 invalid mutation cases
(bad switcher, heading count, code, table, number, file, native/duplicate anchor,
reference target and nested image).
It prints an example rejected malformed fixture and writes no files. This is a
mechanical guard for the repository's plain Markdown, not a complete CommonMark
renderer: translation meaning, spelled-out numbers and factual currency still
need review. It does not change Rust tests or their documented count.
