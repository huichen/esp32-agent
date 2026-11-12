# AGENTS.md · repository conventions

This repository follows the `future-versioning` skill. Load it before changing tracked files.

What follows is what **this repository decides**: scope, branching, commits, the gate and
its boundaries. The skill's default workflow is not repeated here.

## 1. Scope

Two buildable units and one firmware project:

| Path | Contents | Verifiable here |
|---|---|---|
| `crates/agent-core/` | everything board-independent (protocol, HTTP, manifest, drivers, validation, web assets) | ✅ `cargo test` |
| `crates/agent-sim/` | host simulator: the same services on a virtual board | ✅ `cargo run` / `scripts/smoke.ps1`, `scripts/smoke.sh` |
| `firmware/` | ESP-IDF firmware (WiFi, NVS, buses, entry point) | ⚠️ needs the ESP-IDF toolchain, see `docs/flashing.md` |
| `docs/`, `README.md` | documentation | ✅ reviewed together with the code |

`firmware/` is excluded from the workspace (`exclude = ["firmware"]`), so a root
`cargo test` only covers the parts that need no hardware. Generated files (`target/`,
`sim-state/`, `firmware/sdkconfig`, `firmware/build/`) are ignored — never commit them.

## 2. Branching and worktrees

- `main` is always green (`cargo test` passes).
- **Every change happens in a worktree, never by editing `main` directly** — a one-line
  documentation fix included. There is no change small enough to skip the branch: the
  branch is what makes the work reviewable, and rollback is deleting it rather than
  rewriting history someone else may already have.

  ```bash
  git worktree add --no-track .worktrees/<name> -b <type>/<name> github/main
  ```

  `<type>` is `feat` / `fix` / `docs` / `refactor` / `test` / `ci` / `chore`, matching
  the commit prefix.
- `.worktrees/` is untracked. When the change lands: `git worktree remove
  .worktrees/<name>`, delete the branch, `git worktree prune`, then fast-forward local
  `main` from `github/main`. The remote is named `github` in this clone, not `origin`;
  the command above fails with `fatal: invalid reference: origin/main` if copied as a
  habit from another repository.
- Never touch a worktree or branch you did not create. If `main` carries commits you did
  not author, report them; do not clean them up.

## 3. Commits

- One commit, one intent. The message explains **why**, not a restatement of the diff.
  Prefixes: `feat` `fix` `docs` `refactor` `test` `ci` `chore`.
- Changing the manifest format or a validation rule means updating `docs/manifest.md`
  **and** the tests in `crates/agent-core/src/manifest.rs` in the same commit.
- Mixed Chinese/English is fine in the body, but keep the subject in English, lower
  case, so a changelog can be generated later.

## 4. Gate before handoff (all must pass)

Run in this order; if one fails, do not hand off:

```bash
cargo test -p esp32-agent-core          # unit + integration (range, auth, manifest, agent cases)
cargo build -p esp32-agent-sim          # simulator compiles
powershell -File scripts/smoke.ps1      # end to end: portal / tools-list / live manifest / refusal
cargo clippy -p esp32-agent-core -p esp32-agent-sim -- -D warnings
cargo fmt --all --check
```

On macOS or Linux the smoke step is `bash scripts/smoke.sh`, which runs the same commands
in the same order (and needs `python3` for two of its checks). After editing any `.sh`,
`bash -n` every one of them: the parse check is the only part a Windows machine can run.

For documentation-only changes, step 1 still runs (to prove nothing else broke).

Extra requirement for `firmware/`:

```bash
powershell -File scripts/build-firmware.ps1      # needs espup + ESP-IDF, see docs/flashing.md
bash scripts/build-firmware.sh                   # the same, on macOS or Linux
```

Without the toolchain on this machine, **do not claim the firmware is verified**: say
"firmware not built/verified" in the commit message and the handoff, and list what is
suspicious. The bench board and the exact flash command are described in
`docs/flashing.md`; the partition table must be passed to the flasher explicitly.

## 5. Finishing

- Commit on the branch by default; the user decides when to push or open a PR. Do not
  push, open issues or release without being asked.
- The handoff states: files changed, commands run, their results, and what is still
  unverified.
- After a merge, clean up worktrees and branches (§2). Do not leave stale branches.

## 6. Translations

`README.md` is the source. Five translations sit beside it (`README.zh.md`, `README.fr.md`,
`README.es.md`, `README.ru.md`, `README.ar.md`) and are updated **in the same commit** as the
English. The README does not say this; this file does.

- **The switcher is one line, third in the file.** The current language is bold and not a
  link; the other five are links. A translated file must not also link to itself, or the
  language appears twice.
- **In-page anchors are translated.** `(#manifest)` in English is `(#清单)` in Chinese,
  pointing at the translated heading's GitHub slug. Do not carry English anchors over with
  HTML `<a name>` elements: the translations must stay plain Markdown. The cost is that
  renaming an English heading means fixing five link targets in the same commit.
- **Code, commands, JSON, paths, tool names and identifiers are not translated.** Numbers in
  prose are the same in every language.
- **A translation never fixes the English.** If the English is wrong, translate it as written
  and report it; the fix lands in `README.md` and then in all five.
- Check mechanically, not by eye: same `##` heading count, same fenced-block count, same
  table-row count, and every in-page link resolves.
- **The test count moves with the tests.** `README.md` and `ROADMAP.md` both state it, so
  adding a test means updating six READMEs plus `ROADMAP.md` in the same commit. A stale
  number is a false claim about the gate, not a rounding.

## 7. Do not

- Put WiFi passwords or tokens into code, examples or tests. `__set__` in a config or
  manifest is a redaction placeholder, not a value.
- Commit firmware electrical constants (`board.rs`, profiles) straight to `main`: those
  are hardware facts and need evidence (measurement log or datasheet page).
- Change the semantics of `crates/agent-core/src/hardware.rs` to suit one chip; chip
  differences belong in `drivers/`.
- Write "should work" into `docs/`: anything without a build or measurement behind it
  goes into `ROADMAP.md` or is marked unverified.
- Commit `sim-state/`, `firmware/sdkconfig` or any `*.bin`.

## 8. Bench facts the code depends on

Each of these cost a session to find; each is cheap to re-check.

- **The OLED is 128×32, and `oled.height` defaults to 64.** Setup for a 64-row panel
  (`0xA8 0x3F`, COM pins `0x12`) on this glass draws nothing usable. `pins:
  {"oled.height": 32}` in the board's config is what makes it a screen; `GET /api/devices`
  reports the height the firmware is using.
- **The I²S transmit ring has to outlast one OLED frame draw.** At 2 × 200 frames (16.7 ms)
  a 128×32 frame on the I²C bus (about 9 ms) empties the ring, the last descriptor replays,
  and music repeats fragments instead of pausing — measured on the bench: a 30 s clip
  dragged to 33.06 s. 2 × 512 frames (42.7 ms, 4 KB) carries the same clip in 30.19 s;
  8 KB starved the internal heap badly enough that the transcript stopped saving.
- **An I²C glitch on a breadboard does not heal itself.** One interrupted transfer leaves
  the ESP-IDF master answering `ESP_ERR_INVALID_STATE` to every later transfer, and the
  screen is dead until the board is reflashed. `i2c_write`/`i2c_read` now reset the
  peripheral and retry once before reporting.
- **A firmware build from a worktree needs two overrides**: `-TargetDir D:\t` (esp-idf-sys
  rejects output paths over ~88 characters) and
  `ESP_IDF_TOOLS_INSTALL_DIR=custom:<clone>\.embuild\espressif` — the value in
  `firmware/.cargo/config.toml` is relative to the crate, so in a worktree it points at a
  directory that does not exist and the build starts re-cloning ESP-IDF.
- **Flash with the partition table.** `build-firmware.ps1 -Flash` passes
  `firmware/partitions.csv`; a hand-run `espflash flash` has to pass `--partition-table`
  too, or the image lands on the stock single-app table (§4).
