# Build + run helpers (PowerShell)
# Usage:
#   .\build-release.ps1        # check + test + release build
#   .\build-release.ps1 -Run   # also launch the app

param([switch]$Run)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

Write-Host "== cargo check ==" -ForegroundColor Cyan
cargo check
Write-Host "== cargo test ==" -ForegroundColor Cyan
cargo test
Write-Host "== cargo build --release ==" -ForegroundColor Cyan
cargo build --release

$exe = ".\target\release\culvert.exe"
if (Test-Path $exe) {
  Write-Host "Built: $exe" -ForegroundColor Green
  if ($Run) { Start-Process $exe }
} else {
  Write-Host "Build output not found at $exe" -ForegroundColor Yellow
}
