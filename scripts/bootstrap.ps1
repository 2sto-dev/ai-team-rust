$ErrorActionPreference = "Stop"

Write-Host "== AI Team Rust bootstrap =="

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host ""
    Write-Host "Rust/Cargo nu este instalat."
    Write-Host "Instaleaza Rust prin rustup de la https://rustup.rs/ si redeschide terminalul."
    exit 1
}

Write-Host "Cargo:"
cargo --version

Write-Host "Rustc:"
rustc --version

if (-not (Test-Path ".env")) {
    Copy-Item ".env.example" ".env"
    Write-Host "Created .env from .env.example"
}

Write-Host ""
Write-Host "Checking project..."
cargo check

Write-Host ""
Write-Host "Running tests..."
cargo test

Write-Host ""
Write-Host "Bootstrap complete."
Write-Host "Run: cargo run -- doctor"
