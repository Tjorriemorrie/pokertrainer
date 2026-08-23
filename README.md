# pokertrainer

A local poker trainer that replicates the GGPoker 3-max Spin and Gold table
interface and coaches decision-making with a range-based solver.

## What's implemented so far

- **S0 — Project scaffolding:** configuration loading (`.env`), structured
  logging, and a typed error convention.
- **S1 — Database layer:** PostgreSQL schema (opponent profiles, stats,
  contextual ranges, sessions/decisions) with idempotent migrations and an
  in-memory range cache.
- **S2 — Core poker primitives:** card/deck model, a bitboard hand evaluator
  (5- and 7-card), and a deterministic thread-local RNG.
- **S3 — Game state engine:** full 3-max Spin and Gold rules — blinds/button
  rotation, 500-chip starting stacks, street progression, main/side pot
  accounting, and the legal-action flow (fold/check/call/bet/raise/all-in).

The app currently runs as a library plus a small binary that starts up,
connects to the database, and applies migrations. The table UI and solver are
not built yet.

## How to run

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable toolchain).
- PostgreSQL 18 installed at `C:\Program Files\PostgreSQL\18\bin` (used by
  `pg.ps1`).

### 1. Start the database

The project ships a local PostgreSQL instance managed by `pg.ps1` (data lives
in `data/`, port 5433):

```powershell
.\pg.ps1          # init + start + create db/user
.\pg.ps1 status   # check it's running
.\pg.ps1 stop     # stop it
```

### 2. Configure the environment

Copy the example config and adjust if needed:

```powershell
Copy-Item .env.example .env
```

### 3. Run the app

```powershell
cargo run
```

You should see log output like `database ready, migrations up to date`.

### Running the tests

```powershell
cargo test
```

Database integration tests need the local PostgreSQL instance running
(`.\pg.ps1`).
