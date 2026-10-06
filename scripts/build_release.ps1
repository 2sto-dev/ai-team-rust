$ErrorActionPreference = "Stop"

Write-Host "== Building AI Team release =="
cargo test
cargo build --release

$exe = Join-Path $PSScriptRoot "..\target\release\ai-team.exe"
$exe = [System.IO.Path]::GetFullPath($exe)

Write-Host ""
Write-Host "Release binary:"
Write-Host $exe
