## What does this PR do

<!-- one sentence; put the detailed reasoning below -->

## Type

- [ ] New driver (`drivers/*.rs`)
- [ ] Manifest example (`examples/manifests/*.json`)
- [ ] Manifest engine / protocol (`manifest.rs`, `protocol.rs`, `app.rs`)
- [ ] Web console (`web/`)
- [ ] Firmware (`firmware/`)
- [ ] Documentation

## Verification

<!-- paste the commands and the key output, do not just tick the boxes -->

- [ ] `cargo test -p esp32-agent-core` passes
- [ ] `powershell -File scripts/smoke.ps1` passes (or `bash scripts/smoke.sh` on macOS and
      Linux)
- [ ] `cargo clippy … -D warnings` and `cargo fmt --all --check` pass
- [ ] When `firmware/` changed: `scripts/build-firmware.ps1` (or `build-firmware.sh`)
      passes (if there is no toolchain on your machine, say so in the description)

## Hardware evidence (required for driver or board-profile changes)

- Datasheet page / measurement log:
- Board and chip revision tested:

## Documentation

- [ ] `docs/capabilities.md` updated (new driver or capability)
- [ ] `docs/manifest.md` updated (manifest fields changed)

## Blast radius

<!-- Does this change the behaviour of an existing tool? Can an old manifest fail to
load? (new fields must be #[serde(default)]) -->

## Checks

- [ ] No credentials, no `sim-state/`, no `*.bin`, no `firmware/sdkconfig`
- [ ] Nothing unverified written up as supported
