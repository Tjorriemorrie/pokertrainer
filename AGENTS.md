# Project rules

## Rust practices (industry standard)

- Follow idiomatic Rust style: modules in `src/`, one clearly named responsibility per module, `snake_case` naming.
- All public items must compile clean under `cargo clippy -- -D warnings` and `cargo fmt --check`.
- No `unwrap()`/`expect()` or panics in runtime code paths that should propagate errors; use the crate-wide `Result`/`Error` convention from `src/error.rs`.
- No `unsafe`. Use stable Rust only; no nightly features.
- Prefer typed errors (`thiserror`) over boxed/string errors.
- Crates may be used freely when they solve a real problem (e.g. `rand`, `rand_chacha`).
- Log via `tracing` macros; never log secrets or credentials (redact URLs/keys).

## Tests

- Unit/integration tests are standard practice; write them for every module.
- Every module must maintain >= 90% line coverage, verified with `cargo llvm-cov`. Per-module is the gate; all modules of a finished stage must pass before committing.
- The entire test suite must finish within 30 seconds; keep tests fast, reduce or restructure anything slower.
- Database integration tests require the local PostgreSQL instance (start it with `pg.ps1`).
- Tests and manual testing must never run against the real/production database. Use the `_test`-suffixed database (see `db::test_database_url` / `db::test_pool` in `src/db.rs`), which is derived from `DATABASE_URL` and kept separate from real data.
- Verification is: `cargo check`, `cargo clippy -- -D warnings`, `cargo fmt --check`, `cargo test`, `cargo llvm-cov --lib --summary-only` (per-module gate), and a manual `cargo run`.

## UI conventions

- Every navigational or state-changing action that leads to the same destination must use identical label text and identical button styling everywhere it appears, so the user learns one affordance instead of several. E.g. starting/resuming a drill is always an `action-btn pt-secondary` link to `/play`, labeled "Drill" with nothing active or "Resume drill" with one in progress — on the dashboard and on the drill page alike.
- Button styling classes (`assets/style.css`): `pt-confirm` (green) marks a confirming/destructive-adjacent action taken *within* the current page (e.g. "Continue", "Scan for new hand histories"); `pt-secondary` (gold) marks navigation to start or resume play. Reuse these two consistently rather than introducing new button colors.

## Commits

- When a task or stage is finished and verified, commit immediately with a concise message (e.g. "Database layer"); do not wait for the user to ask.
- Stage only intended files; never commit secrets, IDE config (`.idea/`), `data/`, or `target/`.

## README

- After finishing a task or stage, update `README.md` so it always reflects the current state of the app.
- The README must explain how to run the app, assuming the reader has never run a Rust program: prerequisites, exact commands, and any local services (e.g. PostgreSQL via `pg.ps1`).
- The README must give a basic explanation of the features that are implemented so far (only what exists, not planned work).