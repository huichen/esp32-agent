# AGENTS.md · repository conventions

This repository follows the `future-versioning` skill. Load it before changing tracked files.

What follows is what **this repository decides**: scope, branching, commits, the gate and
its boundaries. The skill's default workflow is not repeated here.

## 1. Scope

Two buildable units and one firmware project:

| Path | Contents | Verifiable here |
|---|---|---|
| `crates/agent-core/` | everything board-independent (protocol, HTTP, manifest, drivers, validation, web assets) | ✅ `cargo test` |
| `crates/agent-sim/` | host simulator: the same services on a virtual board | ✅ `cargo run` / `scripts/smoke.ps1` |
| `firmware/` | ESP-IDF firmware (WiFi, NVS, buses, entry point) | ⚠️ needs the ESP-IDF toolchain, see `docs/flashing.md` |
| `docs/`, `README.md` | documentation | ✅ reviewed together with the code |

`firmware/` is excluded from the workspace (`exclude = ["firmware"]`), so a root
`cargo test` only covers the parts that need no hardware. Generated files (`target/`,
`sim-state/`, `firmware/sdkconfig`, `firmware/build/`) are ignored — never commit them.

## 2. Branching and worktrees

- `main` is always green (`cargo test` passes).
- Non-trivial work happens in a worktree, not by editing `main` directly:

  ```bash
  git worktree add --no-track .worktrees/<name> -b <type>/<name> origin/main
  ```

  `<type>` is `feat` / `fix` / `docs` / `refactor` / `test` / `ci` / `chore`, matching
  the commit prefix.
- `.worktrees/` is untracked. When the change lands: `git worktree remove
  .worktrees/<name>`, delete the branch, `git worktree prune`, then fast-forward local
  `main` from `origin/main`.
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

For documentation-only changes, step 1 still runs (to prove nothing else broke).

Extra requirement for `firmware/`:

```bash
powershell -File scripts/build-firmware.ps1      # needs espup + ESP-IDF, see docs/flashing.md
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

## 6. Do not

- Put WiFi passwords or tokens into code, examples or tests. `__set__` in a config or
  manifest is a redaction placeholder, not a value.
- Commit firmware electrical constants (`board.rs`, profiles) straight to `main`: those
  are hardware facts and need evidence (measurement log or datasheet page).
- Change the semantics of `crates/agent-core/src/hardware.rs` to suit one chip; chip
  differences belong in `drivers/`.
- Write "should work" into `docs/`: anything without a build or measurement behind it
  goes into `ROADMAP.md` or is marked unverified.
- Commit `sim-state/`, `firmware/sdkconfig` or any `*.bin`.
