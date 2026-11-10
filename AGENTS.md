# AGENTS.md · 仓库协作约定

This repository follows the `future-versioning` skill. Load it before changing tracked files.

下面写的是**这个仓库自己决定的**部分：范围、分支、提交、验证、边界。技能里的默认流程不重复抄。

## 1. 范围

仓库包含两个可构建单元和一个固件项目：

| 路径 | 内容 | 能否在本机验证 |
|---|---|---|
| `crates/agent-core/` | 与板卡无关的全部逻辑（协议、HTTP、清单、驱动、校验、网页资源） | ✅ `cargo test` |
| `crates/agent-sim/` | 主机模拟器，跑同一套服务 + 虚拟板子 | ✅ `cargo run` / `scripts/smoke.ps1` |
| `firmware/` | ESP-IDF 固件（WiFi、NVS、总线、入口） | ⚠️ 需要 ESP-IDF 工具链，见 `docs/flashing.md` |
| `docs/`, `README.md` | 文档 | ✅ 与代码同步审阅 |

`firmware/` 被 workspace 排除（`exclude = ["firmware"]`），所以根目录的 `cargo test` 只跑不依赖硬件的部分。
生成物（`target/`、`sim-state/`、`firmware/sdkconfig`、`firmware/build/`）已在 `.gitignore` 中排除，不要提交。

## 2. 分支与工作树

- 主分支 `main`，始终可 `cargo test` 通过。
- 非平凡改动在独立工作树里做，不直接在 `main` 上编辑：

  ```bash
  git worktree add --no-track .worktrees/<name> -b <type>/<name> origin/main
  ```

  `<type>` 取 `feat` / `fix` / `docs` / `refactor` / `test` / `ci` / `chore`，与提交前缀一致。
- `.worktrees/` 不入库。完成后 `git worktree remove .worktrees/<name>`、删分支、`git worktree prune`，
  再把本地 `main` 快进到 `origin/main`。
- 不要动别人正在用的工作树或分支；发现 `main` 上有不是你提交的提交，报告，不要清理。

## 3. 提交

- 一个提交一件事；消息说明**为什么**，而不是复述 diff。前缀：`feat:` `fix:` `docs:` `refactor:` `test:` `ci:` `chore:`。
- 改动清单格式或校验规则时，**同一次提交**里同时更新 `docs/manifest.md` 与 `crates/agent-core/src/manifest.rs` 的测试。
- 中英混排即可，但提交标题保持英文、小写开头，便于之后生成 changelog。

## 4. 交付前检查（必须全过）

按顺序执行，任何一步失败就不要交付：

```bash
cargo test -p esp32-agent-core          # 单元 + 集成测试（含越界/鉴权/清单用例）
cargo build -p esp32-agent-sim          # 模拟器可编译
powershell -File scripts/smoke.ps1    # 端到端：网页 / tools-list / 清单热更新 / 越界拒绝
cargo clippy -p esp32-agent-core -p esp32-agent-sim -- -D warnings
cargo fmt --all --check
```

只改文档时，第 1 步仍然要跑一次（确认没顺手改坏代码）。

改 `firmware/` 的额外要求：

```bash
powershell -File scripts/build-firmware.ps1      # 需要 espup + ESP-IDF，见 docs/flashing.md
```

本机没有工具链时，**不要声称固件已验证**：提交信息与交付说明里写明"固件未编译验证"，并把可疑点列出来。

## 5. 完成方式

- 默认在分支上提交，由用户决定何时推送、是否开 PR；未经明确要求不推送、不建 issue、不发版。
- 交付时写清：改了什么文件、跑了哪些命令、结果如何、还有什么没验证。
- 合入后清理工作树与分支（见 §2），不要留下过期分支。

## 6. 不要做的事

- 不把 WiFi 密码、token 一类凭证写进代码、示例或测试；清单与配置里出现 `__set__` 是**脱敏占位**，不是真值。
- 不在 `main` 上直接提交固件电平常量（`board.rs`/`profiles`）——这些是硬件事实，改动需要说明依据（实测记录或数据手册页码）。
- 不改 `crates/agent-core/src/hardware.rs` 的 trait 语义去迁就某一颗芯片；芯片差异属于 `drivers/`。
- 不在 `docs/` 里贴"应该能工作"的结论：没有实测/编译证据的，写进 `ROADMAP.md` 或标注"未验证"。
- 不提交 `sim-state/`、`firmware/sdkconfig`、任何 `*.bin`。
