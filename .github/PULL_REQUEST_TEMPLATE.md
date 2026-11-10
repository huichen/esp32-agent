## 这个 PR 做了什么

<!-- 一句话；详细理由放下面 -->

## 类型

- [ ] 新驱动（`drivers/*.rs`）
- [ ] 清单示例（`examples/manifests/*.json`）
- [ ] 清单引擎/协议（`manifest.rs`、`protocol.rs`、`app.rs`）
- [ ] 网页控制台（`web/`）
- [ ] 固件（`firmware/`）
- [ ] 文档/CI

## 验证

<!-- 贴命令与关键输出，不要只打勾 -->

- [ ] `cargo test -p esp32-agent-core` 通过
- [ ] `powershell -File scripts/smoke.ps1` 通过
- [ ] `cargo clippy … -D warnings` 与 `cargo fmt --all --check` 通过
- [ ] 改到 `firmware/` 时：`scripts/build-firmware.ps1` 通过（没工具链就在描述里说明）

## 硬件依据（改驱动/板型时必填）

- 数据手册页码 / 实测日志：
- 测试过的板子与芯片版本：

## 文档

- [ ] `docs/capabilities.md` 已更新（新驱动/新能力）
- [ ] `docs/manifest.md` 已更新（清单字段变化）
- [ ] `CHANGELOG.md` 已加一行

## 影响面

<!-- 会不会改变已有工具的行为？会不会让旧清单加载失败？（字段必须 #[serde(default)]） -->

## 检查

- [ ] 没有提交凭证、`sim-state/`、`*.bin`、`firmware/sdkconfig`
- [ ] 没有把"未验证"写成"已支持"
