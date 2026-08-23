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
    breakdown, flagged with `intercepted`.
  - Server → Client `CHART_TICK`: one evaluated action appended to the
    top-bar EV curve.

  Each connection owns a live table session that drives the opponents with a
  simple placeholder policy, runs the S6 survivability solver on your
  decisions, and deals the next hand automatically. Opponent ranges are
  uniform until profile/sequence-node loading is wired into the loop.
- **S8 — Blunder intervention engine:** monitors the hero's rolling error
  rate (EV loss per action) and intercepts the worst blunders with a dynamic,
  calibrated threshold. After a 24-action warm-up the trigger is the
  `(1 − p)`-quantile of your own last 300 EV losses, where
  `p = 1/(3 · A_hand)` and `A_hand` is the rolling actions-per-hand ratio —
  tuned so about one hand in three is interrupted. Below the threshold the
  game just continues (the chart still records every EV loss). When the
  threshold is hit, the state transition halts before your action is applied:
  the table freezes behind a *Blunder interrupted* modal showing the blunder
  vs the optimal move. You must press **I understand — continue** (which
  sends `REVIEW_DONE` over the WebSocket) before the held-back action is
  replayed and the game resumes.
- **S9 — Session persistence & EV analytics:** every hero decision is stored
  in the database (`hero_decisions`) with the hand number, street, played and
  optimal action, and the EV lost. The top-bar chart plays back your lifetime
  history: on connect the server sends a decimated `CHART_SNAPSHOT` (100
  points mapping the last 1,000 actions across every table) and keeps it
  refreshed while you play. When you finish a table — press **Finish table**
  in the top bar or just close the tab — the session is finalized, and the
  **Tournament history** link (or `/tournaments`) shows one such graph per
  finished tournament with its hands/actions and average EV loss. Sessions
  without any stored decision never appear.
- **S10 — GGPoker frontend:** a server-rendered GGPoker table skin with the
  Spin and Gold look — dark-teal oval felt on a wooden rail, three fixed seat
  pods (opponents top-left/top-right, hero bottom-center) that stay in place
  when someone folds or busts, gold pot pill, and per-seat bet badges. The
  table sits top-left; the coach feedback panel renders beside it so
  blunder breakdowns never cover the cards. The action dock lives in its own
  right-aligned block directly below the oval — steel-blue **Fold**, green
  **Check/Call**, red **Bet/Raise**, sizing chips (Min / ½-pot / ¾-pot / Pot /
  All-in) labelled in **chip values only** — stacks show their chip count
  with a muted `?` placeholder for the BB equivalent, just like the real
  client (hold **Alt** and the BB values appear, so you still build the
  conversion reflex yourself), the hero pod sits at the bottom edge of the
  felt, and the dock is right-aligned below the oval — and a golden bet
  slider with
  −/+ steppers, mouse-wheel fine grain (wheel ±5, shift-wheel ±25), and a
  synced number box — so the controls never overlap the hero's cards and the
  oval never clips any text. Cards use a **four-color deck** (hearts red,
  diamonds blue, clubs green, spades black) with enlarged rank and suit
  printing. Card deals, chip commits, folds, and hand wins play as
  WebAudio-synthesized sounds (no files — entirely offline), with a 🔊 mute
  toggle persisted in the browser. Styling is pure CSS: no CDN, works with no
  internet connection.

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
serve elsewhere). The table deals automatically — you'll hear the cards land
and the chips move (mute with 🔊 if you prefer silence). When the action dock
appears, either click a sizing chip and the red **Bet/Raise** button, fine-tune
the slider with the mouse wheel, or press **Fold** / **Call** outright — every
amount is in chips. Every decision is charted in the top bar; serious blunders
pause the table and render the played-vs-optimal breakdown in the **coach
feedback** panel to the right of the felt — press **I understand — continue**
to play on. Invalid clicks and reconnects are handled gracefully — the table
keeps dealing.

When you're done with a table, click **Finish table** in the top bar (or just
close the tab): your session is stored, and the **Tournament history** link
in the top bar takes you to <http://127.0.0.1:8744/tournaments>, where every
finished tournament shows the same action-EV graph as the live top bar.

### Running the tests

```powershell
cargo test
```

Database integration tests need the local PostgreSQL instance running
(`.\pg.ps1`).
