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
- **S4 — Range model:** the 169-hand matrix (13×13 grid), GGPoker-style
  bet-sizing buckets (preflop 2bb/3bb/4bb/pot, postflop 1/3/1/2/3/4/pot/
  overbet), Bayesian range filtering, and sequence-node resolution with a
  population fallback.
- **S5 — MCTS solver:** a hero-perspective, range-based search. Opponent
  holdings are sampled from range vectors with blocker (card-removal)
  adjustment; every sampled world keeps its own isolated expectimax-UCT
  search, and action EVs are the range-probability-weighted average across
  worlds.
- **S6 — Decision layer:** validates player-submitted actions (including
  off-bucket bet-slider amounts) and evaluates them against the optimal
  action. Because only first place pays, "optimal" maximizes a survivability
  score derived from CRRA utility over the hero's stack:
  `EV − λσ² − κ·P(bust)` with λ = γ/(2S) and bust cost κ = S·ln(S/b) under
  Kelly (γ = 1) — variance and bust risk are penalized more the shorter the
  hero's stack. The played action's exact chip EV loss is reported against
  the optimal one.

The app currently runs as a library plus a small binary that starts up,
connects to the database, and applies migrations. The table UI is not built
yet.

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
