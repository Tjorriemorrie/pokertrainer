# Poker Trainer — Technical Specification & Implementation Roadmap

## Overview & Architecture Philosophy

The goal is to replicate the exact GGPoker 3-max Spin and Gold table interface while running locally on a single desktop machine. The architecture uses a server-driven UI model: the Rust backend serves rendered HTML fragments over HTTP/WebSockets, swapped dynamically into the DOM, eliminating the need for a JavaScript build system.

The solver models the game strictly from the hero's perspective rather than attempting a global equilibrium. It uses a custom range-based Monte Carlo Tree Search (MCTS) designed to eliminate strategy fusion, paired with a "blunder intervention" engine that interrupts suboptimal decisions to maximize pedagogical value.

## Tech Stack

| Layer | Technology | Purpose |
| --- | --- | --- |
| Web framework | Axum | HTTP routes, static assets, WebSocket connections (local-only). |
| Templating | Server-side HTML template engine | Rendered table-state and modal fragments. |
| Styling | Tailwind CSS (via CDN) | GGPoker dark-themed aesthetic, buttons, layout grid. |
| Table & visuals | HTML5 Canvas / vanilla JS | Oval felt, chip stacks, card-deal animations, action indicators. |
| Range heatmaps | Chart.js / native SVG matrix | 13×13 preflop/postflop range matrices in feedback reviews. |
| Database | PostgreSQL (local, via `pg.ps1`) | Opponent stats, ranges, session/decision persistence. |

Other libraries (async runtime, SQL driver, hand-evaluator, etc.) are chosen during each stage's planning step.

---

## Implementation Index

This list is the resume point for development. Work top-to-bottom; each stage gets its own planning step before implementation.

- [x] **S0 — Project scaffolding:** Cargo crate/bin, `.env` loading, config, logging, error handling.
- [x] **S1 — Database layer:** Migrations for all tables; connection pool; in-memory range caching.
- [x] **S2 — Core poker primitives:** Card/deck model, bitboard hand evaluator, thread-local RNG.
- [x] **S3 — Game state engine:** 3-max Spin and Gold rules, blinds/button, stacks, streets, pots, action flow.
- [x] **S4 — Range model:** 169-hand matrix, GGPoker bet-sizing buckets, Bayesian narrowing, sequence nodes.
- [x] **S5 — MCTS solver:** Range-based search, chance nodes, state isolation, expectimax backprop.
- [x] **S6 — Decision layer:** Action submission, optimal-action selection, evaluation logic.
- [x] **S7 — HTTP + WebSocket bridge:** Axum routes, static assets, templates, WS event protocol.
- [x] **S8 — Blunder intervention engine:** Error-rate calc, dynamic threshold, ~1-in-3-hand calibration.
- [ ] **S9 — Session persistence & analytics:** Decision logging, EV tracker, chart decimation.
- [ ] **S10 — Frontend (server-rendered):** GGPoker table, control panel, feedback modal, top-bar chart.

---

## S0 — Project Scaffolding

Foundation for every later stage.

- Cargo crate with a single binary entry point.
- Load `.env` (e.g. `DATABASE_URL`) and expose a typed configuration.
- Structured logging and a consistent error/`Result` convention used across the crate.
- Local PostgreSQL is provisioned by `pg.ps1` (init/start/stop/reset) on port 5433.

## S1 — Database Layer

**Purpose:** store dynamic player statistics and 13×13 hand-range distributions to feed the real-time MCTS engine.

- **`opponent_profiles`** — base player identities and broad types (e.g. LAG, NIT).
- **`opponent_stats`** — VPIP/PFR/3-Bet/C-Bet percentages, separated by Spin and Gold stack-depth buckets (25 BB, 15 BB, 10 BB); updated after every completed hand.
- **`contextual_ranges`** — maps a node (e.g. `BTN_OPEN_2BB_SB_FOLD`) to a fixed 169-element float array (the 13×13 matrix, AA to 72o) used for in-memory sampling.
- **`hero_sessions` / `hero_decisions`** — session metadata and per-decision EV records (schema in S9).
- Migrations applied idempotently at startup or via a migrate command.
- **Caching strategy:** query Postgres once at hand start, cache the 169-float arrays in an in-memory `RwLock`, and write updated stats back asynchronously post-hand to avoid bottlenecking MCTS rollouts.

## S2 — Core Poker Primitives

Low-level, dependency-free building blocks.

- **Card/deck model:** rank + suit representation, 52-card deck, shuffle.
- **Bitboard hand evaluation:** 64-bit (`u64`) bitboards; evaluate 5- and 7-card holdings in under 10 nanoseconds during rollouts.
- **Thread-local RNG:** deterministic, per-thread randomness for shuffling and sampling.

## S3 — Game State Engine

Full 3-max Spin and Gold rules, hero-perspective.

- 3-max seating: Hero bottom-center, Opponent 1 top-left, Opponent 2 top-right.
- Blind-level escalation, button rotation, 500-chip starting stacks.
- Main/side pot accounting, street progression (preflop/flop/turn/river), and the legal-action flow (fold/check/call/bet/raise/all-in).

## S4 — Range Model

Feeds sampled distributions into the solver.

- **169-hand matrix:** 13×13 row-major grid (AA..22), with index/label/combos mapping.
- **Bet-sizing abstraction:** GGPoker-accurate buckets — preflop 2bb/3bb/4bb/pot, postflop 1/3/1/2/3/4/pot/overbet, plus min and all-in.
- **Sequence nodes:** map game nodes by `(player_id, stack_bucket, abstracted_sequence)`, falling back to population averages when sample size < 30 hands.
- **Bayesian range filtering:** after each Villain action, multiply their 169-hand matrix by conditional action probabilities to narrow their holding distribution.

## S5 — MCTS Solver

Simulation core; quality over speed first, optimize later.

- **Range-based search** from the hero's perspective; no global equilibrium, no strategy fusion.
- **Chance nodes** for hidden information/environmental uncertainty, using range probabilities to compute expected utility.
- **State isolation:** each sampled opponent holding keeps its distinct strategic line, preventing illegal averaging across hidden cards.
- **Probability-weighted backprop:** expected value updated via expectimax principles over sampled opponent hands.

## S6 — Decision Layer

Bridge between engine decisions and the UI.

- **Action submission:** player clicks an action or uses the bet slider; submissions are validated against the legal-action set.
- **Optimal action selection:** survivability-based. The solver reports per-action chip EV, payoff variance, and bust probability; the decision layer ranks candidates by the risk-adjusted score `EV − λσ² − κ·P(bust)`, where λ = γ/(2S) and the bust cost κ = [U(S) − U(b)]/U′(S) come from a CRRA utility of the hero's stack (γ = 1 is Kelly/log utility: κ = S·ln(S/b)). Variance and bust risk are thus penalized harder the shorter the hero's stack — the winner-take-all objective is surviving the longest. The single highest-scoring action is optimal, ties broken strictly by chip EV, then bust probability, then variance.
- **Evaluation logic:** compare a played action against the optimal action to yield an EV loss.

## S7 — HTTP + WebSocket Bridge

Axum-based local server.

- Serve static assets (vanilla JS, CSS) and rendered HTML templates.
- **Event routing over WebSockets:**
  - Client → Server `ACTION_SUBMIT` — played action type + bet-size bucket.
  - Server → Client `TABLE_STATE_UPDATE` — raw state HTML fragments to swap into the DOM.
  - Server → Client `TRIGGER_TACTICAL_OVERLAY` — full tactical-breakdown fragment.
  - Server → Client `CHART_TICK` — updated 1,000-action EV dataset for the top-bar chart.

## S8 — Blunder Intervention Engine

**Goal:** monitor average error rate ($EV_{loss}$) and inject triggers so ~1 in 3 hands features a high-error branch or interrupted suboptimal decision.

- **Evaluation check:** HTMX posts to the evaluate-action endpoint before finalizing table state.
- **Threshold logic:** if $EV_{loss} > \text{Dynamic Threshold}$, halt the state transition and return the Deep Explanation Modal fragment.
- **Interception experience:** game freezes; overlay highlights suboptimal vs. optimal move; player must review before advancing.

## S9 — Session Persistence & EV Analytics

**Purpose:** track long-term decision EV across 1,000+ actions, store hand histories, feed the top-bar chart, and calibrate the intervention trigger.

### Schema (`hero_analytics.sql`)

```sql
CREATE TABLE hero_sessions (
    id SERIAL PRIMARY KEY,
    session_start TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    opponent_1_archetype VARCHAR(30),
    opponent_2_archetype VARCHAR(30)
);

CREATE TABLE hero_decisions (
    id SERIAL PRIMARY KEY,
    session_id INT REFERENCES hero_sessions(id) ON DELETE CASCADE,
    hand_number INT NOT NULL,
    street INT NOT NULL, -- 0: Preflop, 1: Flop, 2: Turn, 3: River
    played_action VARCHAR(20) NOT NULL,
    optimal_action VARCHAR(20) NOT NULL,
    ev_loss FLOAT NOT NULL, -- Calculated by Expectimax (0.0 if optimal)
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_recent_decisions ON hero_decisions(created_at DESC);
```

### Hand-to-action calibration

To keep interventions averaging once per 3rd hand (not every 3rd action), track the rolling actions-per-hand ratio and derive the trigger percentile from it:

$$A_{hand} = \frac{\text{Total actions in last 300 hands}}{\text{Total hands (300)}} \qquad \text{TriggerTargetRatio} = \frac{1}{3 \cdot A_{hand}}$$

An intervention fires only when the current move's $EV_{loss}$ falls into the top percentile defined by that target ratio.

### Charting pipeline

- **Individual tracking:** top-bar line chart plots each decision (x = global action count, y = decision EV loss).
- **Data decimation:** backend sends decimated snapshots (100 points mapping the last 1,000 actions) over WebSockets for instant rendering.

## S10 — Frontend (Server-Rendered)

GGPoker interface replication, served by Rust (no JS build step).

- **Top bar:** hand number, blind level, pot size, dynamic target error-rate metric.
- **Central felt canvas:** 3-max seating rendered with absolute canvas positioning for cards/chips, HTMX overlays for buttons.
- **Action control panel:** fast fold/check/call, smart-sizing bets (0.5 / 0.75 / Pot / All-In / Min-Raise), and a GGPoker-style bet slider with fine-grain wheel control.
- **Feedback drawer/modal:** hidden during play, slides in on flagged errors.
  - **Action comparison:** played vs. optimal action with EV and EV loss.
  - **Opponent range profiling:** 13×13 matrix for the specific 3-max node.
  - **Exploitative rationale:** plain-language explanation (e.g. "Villain's Fold-to-3Bet in SB vs BTN is 68%, far above Nash — 3-bet light to exploit over-folding").
  - **Tree playout viewer:** interactive ISMCTS branch preview of expected future streets.
