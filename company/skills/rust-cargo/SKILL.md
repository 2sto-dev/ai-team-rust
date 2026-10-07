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
- Before using a crate's API, check it with your `rustdocs` tools instead of guessing: cache the
  crate at the version in `Cargo.toml` (`rustdocs__cache_crate`), find the item
  (`rustdocs__search_items_preview` or `rustdocs__search_items_fuzzy`), then read its exact
  signature (`rustdocs__get_item_details`). Caching a crate the first time can take a while; reuse
  what is already cached (`rustdocs__list_cached_crates`).
