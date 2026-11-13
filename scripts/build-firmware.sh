#!/usr/bin/env bash
# Build (and optionally flash) the ESP-IDF firmware.
#
# The POSIX counterpart of scripts/build-firmware.ps1: same toolchain checks, the
# same edit of firmware/.cargo/config.toml, the same cargo and espflash commands.
# Nothing here is hidden — if the toolchain is missing it says exactly which
# command to run.
#
#   bash scripts/build-firmware.sh
#   bash scripts/build-firmware.sh --flash --port /dev/ttyACM0
#   bash scripts/build-firmware.sh --mcpu esp32c3 --target riscv32imc-esp-espidf --flash
#
# Flags mirror the .ps1 parameters: --mcpu, --target, --flash, --port, --debug
# (the .ps1 defaults to -Release; -Release:$false is --debug here), --target-dir.
set -euo pipefail

mcpu=esp32s3
target=xtensa-esp32s3-espidf
flash=0
port=''
release=1
target_dir=''

value_required() {
  if [ "$#" -lt 2 ] || [ -z "$2" ] || [[ "$2" == --* ]]; then
    echo "$1 requires a value" >&2
    exit 2
  fi
}

while [ $# -gt 0 ]; do
  case $1 in
    --mcpu) value_required "$@"; mcpu=$2; shift 2 ;;
    --target) value_required "$@"; target=$2; shift 2 ;;
    --flash) flash=1; shift ;;
    --port) value_required "$@"; port=$2; shift 2 ;;
    --debug) release=0; shift ;;
    --target-dir) value_required "$@"; target_dir=$2; shift 2 ;;
    -h|--help)
      awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"
      exit 0 ;;
    *) echo "unknown flag $1" >&2; exit 2 ;;
  esac
done

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
fw="$repo/firmware"

# Resolve the repository before stamping metadata, and export both fields.
ESP32_AGENT_COMMIT="$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)"
ESP32_AGENT_BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
export ESP32_AGENT_COMMIT ESP32_AGENT_BUILT_AT

need() {
  if ! command -v "$1" > /dev/null 2>&1; then
    echo "missing \`$1\` — $2" >&2
    exit 1
  fi
}

need cargo 'install Rust: https://rustup.rs'
need rustup 'install Rust: https://rustup.rs'
if ! rustup toolchain list | grep -q '^esp\([[:space:]]\|$\)'; then
  echo 'missing the esp toolchain — run: cargo install espup; espup install' >&2
  exit 1
fi
if ! command -v ldproxy > /dev/null 2>&1; then
  echo 'missing ldproxy — run: cargo install ldproxy' >&2
  exit 1
fi
if ! command -v espflash > /dev/null 2>&1; then
  echo 'warning: espflash not found: build will work, flashing will not. cargo install espflash' >&2
fi

# The toolchain config in firmware/.cargo/config.toml points at S3; keep them in
# sync, and write only when something actually changed, so the file does not show
# a one-line diff after every build.
cfg="$fw/.cargo/config.toml"
before=$(cat "$cfg")
text=$(printf '%s\n' "$before" \
  | sed -e "s|target = \"[^\"]*\"|target = \"$target\"|" \
        -e "s|MCU = \"[^\"]*\"|MCU = \"$mcpu\"|" \
        -e "s|runner = \"[^\"]*\"|runner = \"espflash flash --monitor\"|")
if [ "$text" != "$before" ]; then
  printf '%s\n' "$text" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
fi

cd "$fw" || exit 1

if [ -f "$HOME/export-esp.sh" ]; then
  echo 'sourcing export-esp.sh'
  # shellcheck disable=SC1091
  . "$HOME/export-esp.sh"
elif [ -f "$HOME/export-esp.ps1" ]; then
  echo 'warning: export-esp.ps1 found but this is a POSIX shell; ensure the esp toolchain env vars are set' >&2
else
  echo 'warning: export-esp.sh not found — run `espup install` once, or set PATH manually' >&2
fi

# bindgen (esp-idf-sys' first run) wants the directory holding libclang, but
# export-esp.sh can point LIBCLANG_PATH at the library itself; the first build then
# fails with "couldn't find libclang" instead of anything actionable.
if [ -n "${LIBCLANG_PATH:-}" ] && [ -f "$LIBCLANG_PATH" ]; then
  LIBCLANG_PATH=$(dirname "$LIBCLANG_PATH")
  export LIBCLANG_PATH
fi

# The `esp` toolchain carries the xtensa/riscv target and build-std; the default
# one carries neither and the failure reads as `can't find crate for core`.
#
# A machine-wide `rustc-wrapper = "sccache"` breaks esp-idf-sys: its rustc
# invocation carries thousands of `--cfg` flags and sccache fails to spawn it.
# Cargo does not let an environment variable turn a configured wrapper off, so this
# has to be said out loud before it costs someone an hour. `cargo config get` is
# unstable, so probe it quietly on a stable channel.
wrapped=$(cargo +esp config get build.rustc-wrapper 2> /dev/null || true)
case $wrapped in
  *sccache*)
    echo "warning: cargo is configured with rustc-wrapper = $wrapped; esp-idf-sys fails with 'os error 206' because its command line is too long. Remove the [build] rustc-wrapper line from the cargo config for this build." >&2 ;;
esac

if [ -n "$target_dir" ]; then
  CARGO_TARGET_DIR="$target_dir"
elif [ -z "${CARGO_TARGET_DIR:-}" ]; then
  CARGO_TARGET_DIR="$repo/target"
fi
export CARGO_TARGET_DIR

# esp-idf-sys refuses an output directory over 88 characters on Windows; the
# warning is cheap and saves an afternoon on a deep clone.
out_probe="$CARGO_TARGET_DIR/$target/release/build/esp-idf-sys-0000000000000000/out"
if [ "${#out_probe}" -gt 84 ]; then
  echo "warning: the output path is ${#out_probe} characters; esp-idf-sys refuses more than 88 — pass --target-dir with a shorter path" >&2
fi

build_args=(+esp build --target "$target")
profile_name=release
if [ "$release" -eq 1 ]; then
  build_args+=(--release)
else
  profile_name=debug
fi
echo "cargo ${build_args[*]} (MCU=$mcpu, target-dir=$CARGO_TARGET_DIR)"
cargo "${build_args[@]}" || { echo 'firmware build failed' >&2; exit 1; }

if [ "$flash" -eq 1 ]; then
  need espflash 'cargo install espflash'
  bin="$CARGO_TARGET_DIR/$target/$profile_name/esp32-agent"
  if [ ! -f "$bin" ]; then echo "built firmware not found: $bin" >&2; exit 1; fi
  table="$fw/partitions.csv"
  if [ ! -f "$table" ]; then echo "partition table not found: $table" >&2; exit 1; fi
  flash_args=(flash --monitor "$bin" --partition-table "$table" --target-app-partition factory)
  if [ -n "$port" ]; then flash_args+=(--port "$port"); fi
  echo "espflash ${flash_args[*]}"
  espflash "${flash_args[@]}" || { echo 'firmware flash failed' >&2; exit 1; }
else
  echo 'build ok — flash with: bash scripts/build-firmware.sh --flash --port /dev/ttyACM0'
fi
