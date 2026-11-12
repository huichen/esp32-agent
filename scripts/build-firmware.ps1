<#
.SYNOPSIS
  Build (and optionally flash) the ESP-IDF firmware.
.DESCRIPTION
  Wraps the esp-rs toolchain: checks that the `esp` toolchain, ldproxy and a valid
  target are present, then runs cargo inside firmware/. Nothing here is hidden —
  if the toolchain is missing it says exactly which command to run.
.EXAMPLE
  powershell scripts/build-firmware.ps1
  powershell scripts/build-firmware.ps1 -Flash -Port COM3
  powershell scripts/build-firmware.ps1 -Mcpu esp32c3 -Target riscv32imc-esp-espidf -Flash
#>
param(
  [string]$Mcpu = 'esp32s3',
  [string]$Target = 'xtensa-esp32s3-espidf',
  [switch]$Flash,
  [string]$Port = '',
  [switch]$Release = $true,
  # esp-idf-sys refuses an output directory longer than 88 characters on Windows and
  # tells you to keep the project path short. `firmware/target` blows past that in any
  # clone with a name of its own, so artifacts go here instead (a path under the clone
  # root, already gitignored). Override with a shorter path in a deep clone, e.g.
  # -TargetDir D:\t
  [string]$TargetDir = ''
)

# Which commit this image came from, and when. Read back from /api/status so "did the
# flash take" is a comparison of two strings instead of an inference from the tool list.
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$fw = Join-Path $repo 'firmware'

# Pinned after $repo exists: `git -C $null` answers nothing, and an image that says
# "unknown" is exactly the situation this field was added to end.
$env:ESP32_AGENT_COMMIT = (git -C $repo rev-parse --short HEAD 2>$null)
if (-not $env:ESP32_AGENT_COMMIT) { $env:ESP32_AGENT_COMMIT = 'unknown' }
$env:ESP32_AGENT_BUILT_AT = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

function Need($cmd, $hint) {
  if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
    throw "missing `$cmd` — $hint"
  }
}

Need 'cargo' 'install Rust: https://rustup.rs'
$toolchains = (rustup toolchain list) -join "`n"
if ($toolchains -notmatch 'esp') {
  throw 'missing the esp toolchain — run: cargo install espup; espup install'
}
if (-not (Get-Command 'ldproxy' -ErrorAction SilentlyContinue)) {
  throw 'missing ldproxy — run: cargo install ldproxy'
}
if (-not (Get-Command 'espflash' -ErrorAction SilentlyContinue)) {
  Write-Warning 'espflash not found: build will work, flashing will not. cargo install espflash'
}

# The toolchain config in firmware/.cargo/config.toml points at S3; keep them in sync.
# Written only when something actually changed, and without a BOM: `Set-Content -Encoding
# utf8` on PowerShell 5.1 stamps one, which then shows up as a one-line diff in every
# build (and `cargo fmt --check` has no opinion about it).
$cfg = Join-Path $fw '.cargo\config.toml'
$before = [System.IO.File]::ReadAllText($cfg, [System.Text.Encoding]::UTF8)
$text = $before -replace 'target = "[^"]+"', "target = `"$Target`""
$text = $text -replace 'MCU = "[^"]+"', "MCU = `"$Mcpu`""
$text = $text -replace 'runner = "[^"]+"', "runner = `"espflash flash --monitor`""
if ($text -ne $before) {
  $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
  [System.IO.File]::WriteAllText($cfg, $text, $utf8NoBom)
}

Push-Location $fw
try {
  if (Test-Path "$env:USERPROFILE\export-esp.ps1") {
    Write-Host 'sourcing export-esp.ps1' -ForegroundColor DarkGray
    . "$env:USERPROFILE\export-esp.ps1"
  } elseif (Test-Path "$env:USERPROFILE\export-esp.sh") {
    Write-Warning 'export-esp.sh found but this is PowerShell; ensure the esp toolchain env vars are set'
  } else {
    Write-Warning 'export-esp.ps1 not found — run `espup install` once, or set PATH manually'
  }

  # bindgen (esp-idf-sys' first run) wants the directory holding libclang.dll, but
  # export-esp.ps1 points LIBCLANG_PATH at the DLL itself; the first build then fails
  # with "couldn't find libclang" instead of anything actionable.
  if ($env:LIBCLANG_PATH -and (Test-Path $env:LIBCLANG_PATH -PathType Leaf)) {
    $env:LIBCLANG_PATH = Split-Path -Parent $env:LIBCLANG_PATH
  }

  # The `esp` toolchain carries the xtensa/riscv target and build-std; the default one
  # carries neither and the failure reads as `can't find crate for core`.
  #
  # A machine-wide `rustc-wrapper = "sccache"` (D:\.cargo\config.toml on this bench) breaks
  # esp-idf-sys on Windows: its rustc invocation carries thousands of `--cfg` flags, and
  # sccache fails to spawn it with `os error 206` (the command line is too long). Cargo
  # does not let an environment variable turn a configured wrapper off, so this has to be
  # said out loud before it costs someone an hour.
  $wrapped = ''
  # `cargo config get` is unstable, so on a stable channel it exits non-zero and writes a
  # message to stderr. Under `$ErrorActionPreference = 'Stop'` that aborts the whole
  # script, which is worse than the warning it was meant to print: probe it quietly.
  $saved = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    $wrapped = (& cargo +esp config get build.rustc-wrapper 2>$null | Out-String)
  } catch {
    $wrapped = ''
  }
  $ErrorActionPreference = $saved
  if ($wrapped -match 'sccache') {
    Write-Warning "cargo is configured with rustc-wrapper = $wrapped; esp-idf-sys fails on Windows with 'os error 206' because its command line is too long. Move the [build] rustc-wrapper line out of the cargo config for this build."
  }
  if (-not $env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR = if ($TargetDir) { $TargetDir } else { Join-Path $repo 'target' }
  }
  $outProbe = "$env:CARGO_TARGET_DIR\$Target\release\build\esp-idf-sys-0000000000000000\out"
  if ($outProbe.Length -gt 84) {
    Write-Warning "the output path is $($outProbe.Length) characters; esp-idf-sys refuses more than 88 on Windows - pass -TargetDir with a shorter path"
  }

  $profileArg = if ($Release) { '--release' } else { '' }
  Write-Host "cargo +esp build $profileArg --target $Target (MCU=$Mcpu, target-dir=$env:CARGO_TARGET_DIR)" -ForegroundColor Cyan
  cargo +esp build $profileArg --target $Target
  if ($LASTEXITCODE -ne 0) { throw 'firmware build failed' }

  if ($Flash) {
    Need 'espflash' 'cargo install espflash'
    $bin = Join-Path $env:CARGO_TARGET_DIR "$Target\release\esp32-agent"
    if (-not (Test-Path $bin)) { $bin = Join-Path $env:CARGO_TARGET_DIR "$Target\debug\esp32-agent" }
    $args = @('flash', '--monitor', $bin)
    if ($Port) { $args += @('--port', $Port) }
    Write-Host "espflash $($args -join ' ')" -ForegroundColor Cyan
    & espflash @args
  }
} finally {
  Pop-Location
}

if (-not $Flash) {
  Write-Host 'build ok — flash with: powershell scripts/build-firmware.ps1 -Flash -Port COM3' -ForegroundColor Green
}
