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
  [switch]$Release = $true
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$fw = Join-Path $repo 'firmware'

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
$cfg = Join-Path $fw '.cargo\config.toml'
$text = Get-Content $cfg -Raw
$text = $text -replace 'target = "[^"]+"', "target = `"$Target`""
$text = $text -replace 'MCU = "[^"]+"', "MCU = `"$Mcpu`""
$text = $text -replace 'runner = "[^"]+"', "runner = `"espflash flash --monitor`""
Set-Content -Path $cfg -Value $text -Encoding utf8

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

  $profileArg = if ($Release) { '--release' } else { '' }
  Write-Host "cargo build $profileArg --target $Target (MCU=$Mcpu)" -ForegroundColor Cyan
  cargo build $profileArg --target $Target
  if ($LASTEXITCODE -ne 0) { throw 'firmware build failed' }

  if ($Flash) {
    Need 'espflash' 'cargo install espflash'
    $bin = Join-Path $fw "target\$Target\release\esp32-agent"
    if (-not (Test-Path $bin)) { $bin = Join-Path $fw "target\$Target\debug\esp32-agent" }
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
