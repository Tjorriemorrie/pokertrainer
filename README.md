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
- **S7 — HTTP + WebSocket bridge:** an Axum server (`127.0.0.1:8744` by
  default, `SERVER_ADDR` to change) serving the rendered app shell and static
  assets (`assets/`), plus a WebSocket at `/ws` with the event protocol:
  - Client → Server `ACTION_SUBMIT`: an action type plus a bet-size bucket
    (or exact slider amount), resolved server-side against the legal set.
  - Server → Client `TABLE_STATE_UPDATE`: the table HTML fragment (seats,
    stacks, board, action buttons, log) swapped into the DOM each turn.
  - Server → Client `TRIGGER_TACTICAL_OVERLAY`: the played-vs-optimal
    breakdown. Currently fires on any suboptimal action; S8 replaces this
    with the calibrated ~1-in-3-hand interception.
  - Server → Client `CHART_TICK`: one evaluated action for the top-bar EV
    tracker (the decimated 1,000-action dataset arrives in S9).

  Each connection owns a live table session that drives the opponents with a
  simple placeholder policy, runs the S6 survivability solver on your
  decisions, and deals the next hand automatically. Opponent ranges are
  uniform until profile/sequence-node loading is wired into the loop.

The app starts the game server immediately after connecting to the database;
open the address below in a browser and play. The polished GGPoker table
interface itself is the S10 scope.

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

You should see log output like `database ready, migrations up to date`
followed by `pokertrainer table server listening`.

### 4. Play

Open <http://127.0.0.1:8744> in a browser (change `SERVER_ADDR` in `.env` to
serve elsewhere). The table deals automatically: when the yellow action
buttons appear, click one (or enter a bet amount and press **Bet amount**).
Suboptimal plays open a tactical overlay that shows the optimal move and the
EV lost; the bar at the top charts your EV loss per action. Invalid clicks
and reconnects are handled gracefully — the table keeps dealing.

### Running the tests

```powershell
cargo test
```

Database integration tests need the local PostgreSQL instance running
(`.\pg.ps1`).
