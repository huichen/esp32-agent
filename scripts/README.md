# `scripts/` — what a person or CI runs

Everything here runs on a development machine, with no hardware attached. That is
the test: if a script in this directory needed a board on the desk, it would live in
[`tools/`](../tools/) instead.

| Script | What it does | Needs |
|---|---|---|
| `smoke.ps1` / `smoke.sh` | end-to-end gate against the host simulator: portal, `tools/list`, a live manifest change, an out-of-range call that must be refused | `cargo`, `curl`, `python3` |
| `build-firmware.ps1` / `build-firmware.sh` | build the ESP-IDF firmware, optionally flash it | `espup` + ESP-IDF, see [flashing](../docs/flashing.md) |

Each script exists twice, once in PowerShell and once in POSIX `sh`, and the two run
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
moves the artifacts when the clone path is too long for `esp-idf-sys`.
