# Project rules

## Rust practices (industry standard)

- Follow idiomatic Rust style: modules in `src/`, one clearly named responsibility per module, `snake_case` naming.
- All public items must compile clean under `cargo clippy -- -D warnings` and `cargo fmt --check`.
- No `unwrap()`/`expect()` or panics in runtime code paths that should propagate errors; use the crate-wide `Result`/`Error` convention from `src/error.rs`.
- No `unsafe`. Use stable Rust only; no nightly features.
- Prefer typed errors (`thiserror`) over boxed/string errors; keep dependencies minimal per TECHNICAL.md.
- Log via `tracing` macros; never log secrets or credentials (redact URLs/keys).

## Tests

- This repo requires no tests. Do not write unit/integration tests unless explicitly asked.
- Verification is: `cargo check`, `cargo clippy -- -D warnings`, `cargo fmt --check`, and a manual `cargo run`.

## Commits

- When a task or stage is finished and verified, commit immediately with a concise message (e.g. "S1 — Database layer"); do not wait for the user to ask.
- Stage only intended files; never commit secrets, IDE config (`.idea/`), `data/`, or `target/`.