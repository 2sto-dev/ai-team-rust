---
name: Rust with cargo
description: Idiomatic Rust crates with unit and integration tests run by cargo test.
---

- Edition 2021 or newer; `cargo build` must pass without warnings, and `cargo clippy` should.
- Library code returns `Result`; no `unwrap()`/`expect()` outside tests and truly impossible cases.
- Unit tests in a `#[cfg(test)] mod tests` next to the code; integration tests in `tests/`.
- Keep `Cargo.toml` dependencies minimal and pinned to major versions; do not add a crate for
  something the standard library does.
- Prefer borrowing over cloning, `&str` over `String` in parameters, and small modules.
- Async only when the project already uses an async runtime (usually tokio).
