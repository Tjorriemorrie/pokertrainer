# pokertrainer

A local poker trainer that replicates the GGPoker 3-max Spin and Gold table
interface and coaches decision-making with a range-based solver.

## Features

- **Dashboard & live-tournament persistence:** the app starts on a dashboard.
  When no tournament is open it offers **Start tournament**; while one is open
  it shows a **Resume tournament** card (hand number, street, blinds, your
  stack, opponents left, actions played) and hides the start button — exactly
  one tournament is live at a time, and a new one can only begin once the
  previous one ends. A tournament ends by winning or losing it, or by
  pressing **Finish table**, which counts as giving up and is recorded as a
  LOSS. Closing the tab (or restarting the app) does *not* end the
  tournament: the active table's full state — street, turn, exact bet sizes,
  stacks, board, hole cards, the undealt deck order, the action log, opponent
  HUD counters, and the hand/action counters — is snapshotted into the
  database after every change, so reconnecting resumes the very same hand
  where you left it. The blunder-intervention history is rebuilt from the
  stored decisions, so the coach's interception threshold also picks up where
  it stopped. A second browser tab cannot claim the same table; it is sent
  back to the dashboard while the first one plays.
- **Project scaffolding:** configuration loading (`.env`), structured
  logging, and a typed error convention.
- **Database layer:** PostgreSQL schema (opponent profiles, stats,
  contextual ranges, sessions/decisions) with idempotent migrations and an
  in-memory range cache.
- **Core poker primitives:** card/deck model, a bitboard hand evaluator
  (5- and 7-card), and a deterministic thread-local RNG.
- **Game state engine:** full 3-max Spin and Gold rules — blinds/button
  rotation, street progression, main/side pot accounting, and the
  legal-action flow (fold/check/call/bet/raise/all-in). Each new drill is
  seated with the most common starting stack of your last 11 imported
  tournaments (300 chips when no history exists yet).
- **Range model:** the 169-hand matrix (13×13 grid), GGPoker-style
  bet-sizing buckets (preflop 2bb/3bb/4bb/pot, postflop 1/3/1/2/3/4/pot/
  overbet), Bayesian range filtering, and sequence-node resolution with a
  population fallback.
- **MCTS solver:** a hero-perspective, range-based search. Opponent
  holdings are sampled from range vectors with blocker (card-removal)
  adjustment; every sampled world keeps its own isolated expectimax-UCT
  search, and action EVs are the range-probability-weighted average across
  worlds. The budget scales with the street: preflop gets 2× the worlds,
  2× the iterations, and one extra tree-depth cap (flop 1.5×, turn 1.25×,
  river unchanged) because early streets branch over far more unknown
  runouts. Every solve reports how deep it actually went — worlds,
  iterations, realized tree depth, nodes, and rollout actions — and the
  coach panel shows this alongside per-action visit counts so the search
  effort can be audited.
- **Background solver with tree reuse:** the MCTS no longer runs as a
  one-shot solve on each click. A persistent background worker starts
  searching the moment the table is dealt (the table renders first, then the
  solver spins up) and keeps refining the current decision in small chunks
  while you think. When you act, the search trees are **reshaped onto the
  played branch** — the hero's action and the opponents' replies are followed
  down the existing trees, so visits and value sums survive into the next
  decision instead of being thrown away. A new street or hand resamples the
  opponent worlds (so holdings never clash with the dealt board), and
  submissions answer instantly from the latest snapshot (an off-bucket
  slider amount falls back to a full inline solve). The action dock's
  top-left corner shows a live **search-depth badge** — `d5/5 · 71k` — that
  reads the realized tree depth against the planned cap and the number of
  iterations run (in thousands). It turns **red** while the search is still
  working toward its depth budget, **orange** once the depth is reached but
  the minimum think time (five seconds) has not elapsed yet, and **green**
  when both are met. The solver does not stop at green: it keeps deepening
  the tree until the decision's wall budget (20 seconds) has elapsed or the
  player acts, whichever comes first. Until the badge is green the action
  dock is **locked** — its buttons are hidden behind a "simulating" hint so
  you can never click an action the search has not finished evaluating.
  Every status frame carries the decision it belongs to (hand, action count,
  street), so stale updates queued behind a reshaped tree can never paint
  the wrong badge or unlock the dock early.
- **Decision layer:** validates player-submitted actions (including
  off-bucket bet-slider amounts) and evaluates them against the optimal
  action. Because only first place pays, "optimal" maximizes a survivability
  score derived from CRRA utility over the hero's stack:
  `EV − λσ² − κ·P(bust)` with λ = γ/(2S) and bust cost κ = S·ln(S/b) under
  Kelly (γ = 1) — variance and bust risk are penalized more the shorter the
  hero's stack. The played action's EV loss is normalized to **big blinds**,
  so a preflop mistake counts as heavily as an equally bad river mistake
  regardless of pot size.
- **HTTP + WebSocket bridge:** an Axum server (`127.0.0.1:8744` by
  default, `SERVER_ADDR` to change) serving the rendered app shell and static
  assets (`assets/`), plus a WebSocket at `/ws` with the event protocol:
  - Client → Server `ACTION_SUBMIT`: an action type plus a bet-size bucket
    (or exact slider amount), resolved server-side against the legal set.
  - Server → Client `TABLE_STATE_UPDATE`: the table HTML fragment (seats,
    stacks, board, action buttons, log) swapped into the DOM each turn.
  - Server → Client `TRIGGER_TACTICAL_OVERLAY`: the played-vs-optimal
    breakdown, flagged with `intercepted`. The candidate table is always
    sorted cheapest-first — fold leads when it is an action, then check,
    call, bets/raises by size, and all-in last — and the raw EV numbers sit
    below a plain-English sentence that reads the EV gap the way a player
    would ("That one adds up: Call gives up about 0.9 BB versus Raise to 120
    every time this spot repeats.").
  - Server → Client `CHART_TICK`: one evaluated action appended to the
    top-bar EV curve.
  - Server → Client `SEARCH_STATUS`: the background solver's live progress
    (iterations done, realized tree depth, nodes, phase, and the decision
    token it belongs to) that drives the depth badge and the action-dock
    lock in the UI.

  Each connection owns a live table session that drives the opponents with a
  simple placeholder policy, runs the survivability solver on your
  decisions, and deals the next hand automatically. Opponent ranges are
  uniform until profile/sequence-node loading is wired into the loop.
- **Opponent HUD in the coach panel:** every opponent action is fed into a
  live tracker, so whenever the coach renders a breakdown the panel also
  shows what it has learned about each opponent — hands dealt, VPIP, PFR,
  folds-to-bet, and postflop aggression, plus a one-line read (e.g. "Loose
  passive — plays many hands and calls instead of raising", with a
  small-sample disclaimer until five hands exist). Each opponent card carries
  their position badges (BTN / SB / BB), a Folded / All-in / Busted status,
  and the stack pill with the same `?`/Alt BB reveal as the table.
- **Plain-language search effort:** the raw solver telemetry (worlds ×
  iterations, tree depth, nodes, rollout actions) is replaced by a color
  grade — **Quick**, **Solid**, or **Deep** search — and one everyday
  sentence such as *"Played out 64 possible opponent hands × 256 evaluations
  each, thinking up to 4 moves ahead — 30.2k simulated actions"*. A short
  confidence note explains whether that is a lot or a little, and the raw
  numbers remain available in the hover tooltip.
- **Blunder intervention engine:** monitors the hero's rolling error
  rate (EV loss per action, in big blinds so pot size never skews severity)
  and intercepts the worst blunders with a dynamic, calibrated threshold.
  After a 24-action warm-up the trigger is the
  `(1 − p)`-quantile of your own last 300 EV losses, where
  `p = 1/(3 · A_hand)` and `A_hand` is the rolling actions-per-hand ratio —
  tuned so about one hand in three is interrupted. Below the threshold the
  game just continues (the chart still records every EV loss). When the
  threshold is hit, the state transition halts before your action is applied:
  the table freezes behind a *Blunder intercepted* review showing the blunder
  vs the optimal move. Press **Continue** (which sends `REVIEW_DONE` over the
  WebSocket) and the coach's best-EV action is played for you — your original
  click never reaches the table, but the blunder itself stays recorded in the
  chart and stored history, so you can see how much this instinctive play
  would have cost.
- **Session persistence & EV analytics:** every hero decision is stored
  in the database (`hero_decisions`) with the hand number, street, played and
  optimal action, and the EV lost (in big blinds). The top-bar chart plays
  back your lifetime
  history: on connect the server sends a decimated `CHART_SNAPSHOT` (100
  points mapping the last 1,000 actions across every table) and keeps it
  refreshed while you play. When you finish a table — press **Finish table**
  in the top bar, which gives up and records a LOSS — or the tournament ends
  naturally, the session is finalized, and the
  **Tournament history** link (or `/tournaments`) shows one such graph per
  finished tournament with its hands/actions and average EV loss in BB.
  The listing is **paginated** (25 per page, newest first) with
  **← Newer / Older →** controls driven by `?page=`, so the whole history
  stays browsable no matter how many tables you have played. Sessions
  without any stored decision never appear.
- **Tournament completion & detail page:** a tournament ends the moment you
  lose your last chip — the bots stop right there, with the chip-leading
  bot recorded as the winner — or when a
  single seat is left standing. Busted opponents are **out** — they stop being
  dealt cards, stop posting blinds, and are skipped in the action order, with
  an **OUT** badge over their seat. An out player never receives cards at a
  later showdown: only the seats still in the hand reveal hands and contest
  pots, and the solver's opponent sampling skips eliminated seats entirely
  (they hold placeholder cards in the search, no runout cards are burned
  for them, and they can never win a pot). When the tournament ends, the table
  **stops**: no further hand is dealt, the connection does not restart, and a
  winner/loser
  modal appears (gold for a win, red for a loss) whose **Continue** button
  takes you to that tournament's detail page (`/tournaments/{id}`), which
  shows the outcome and final stack, hands played, hands won/lost, win rate,
  all-in frequency, average/total EV loss, and the biggest blunder — for any
  past tournament, not just the one you just finished.
- **GGPoker hand-history import:** your real PokerCraft exports zip files
  dropped into the `history/` folder are scanned and imported into the
  database. The dashboard and table headers link to **Hand history**
  (`/history`), which starts with a **Scan for new hand histories** button.
  Scanning reads every zip in the folder (already-imported hands are skipped,
  so re-scanning is always safe) and then shows a **results page** whose
  stats cover *only the newly imported hands* — hands won/lost, win ratio,
  all-ins, showdowns, and chips won/lost. Back on the history page, the
  imported tournaments are listed newest first with buy-in, place, prize,
  profit, hands, and win rate, under lifetime totals: tournaments won, hand
  win ratio, all-ins, total profit, and net chips. Each tournament links to
  its detail page, which shows every hand (time, blinds, position, cards,
  all-in/showdown, invested vs collected chips, result, board) plus the
  tournament's money outcome. Profit is real money - the buy-in and prize
  come from the tournament-summary files PokerCraft exports next to the
  hands.
- **Opponent-skill analysis & skill-graded bots:** the hand history page's
  **Analyze imported opponents** button replays every opponent decision in
  the last 1,000 imported hands and grades it against a seat-perspective
  MCTS solve, scoring each played action's big-blind EV loss. The pooled
  field average becomes one skill level (0..1, anchored at 5 BB lost per
  decision) with a per-opponent breakdown and a background job with live
  progress; per-hand results are cached in the database so re-runs are
  incremental. Saving the **bot template** makes both local bots play via
  their own MCTS solves with a skill-tempered action spread (skill 1 always
  takes the solver-best action; weaker skills leak big blinds like the
  analyzed field), and the table header chip — **You 0.77 · Bots 0.48** —
  compares your lifetime skill against the field on the same scale.
- **GGPoker frontend:** a server-rendered GGPoker table skin with the
  Spin and Gold look — dark-teal oval felt on a wooden rail, three fixed seat
  pods (opponents top-left/top-right, hero bottom-center) that stay in place
  when someone folds or busts, gold pot pill, and per-seat bet badges: every
  player's current street bet (blinds included) shows as a gold chip pill in
  front of their pod — hero included — and is swept into the pot pill as soon
  as the betting round closes. An **action log** is docked to the left of the
  oval, exactly as tall as the table, always visible: every hand deal names
  the button (`— Hand #3 — blinds 10/20 — BTN Opponent 2`) and logs both
  blind posts (`Opponent 2 post SB 10`, `Opponent 1 post BB 20`), so every
  chip each seat commits — blinds included — is on the record, plus folds,
  calls, bets, raises, the dealt board (each street's cards are logged as
  they land — `Flop 2c 7h Kd`, `Turn 4s`, `River Jd`), and results are
  appended below earlier entries and the
  panel auto-scrolls to the newest line. At showdown the full board is logged
  (`Board 2c 7h Kd 4s Jd`), then every player's revealed
  cards (with their hand class) are logged, followed by a line stating who
  won (name, amount, and the winning hand class) or how the pot was split,
  so the log tells the whole story of the hand. When a hand ends, no centre
  banner
  appears: a gold **WIN** ribbon drops over the winner's seat showing the
  exact amount they took down (and the win jingle plays), stays for about two
  seconds, and then the next hand is dealt automatically. Amounts render
  larger: stack pills are as big as the pot pill, and the pot pill and the
  street-bet chips on the felt are one size larger than before. The
  table sits top-left; meaning the coach panel never hides the cards. The page is fluid: on wide
  screens the right column stretches and on narrow ones it wraps below the
  table. The action dock lives in its own
  right-aligned block directly below the oval — steel-blue **Fold**, green
  **Check/Call**, red **Bet/Raise**, sizing chips (Min / ½-pot / ¾-pot / Pot /
  All-in) labelled in **chip values only** — stacks show their chip count
  with a muted `?` placeholder for the BB equivalent, just like the real
  client (hold **Alt** and the BB values appear, so you still build the
  conversion reflex yourself), seat pods carry no avatar icons — just name,
  cards and stack — and the active seat's name glows gold; the hero pod sits
  at the bottom edge of the felt, and the dock is right-aligned below the
  oval — and a golden bet slider with
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
serve elsewhere). You land on the **dashboard**: press **Start tournament**
to play (when one is already open you get a **Resume tournament** card
instead, and starting a new one is impossible until the current one ends —
win it, lose it, or give up with **Finish table**). Both buttons open the
table. The table deals automatically — you'll hear the cards land and the
chips move (mute with 🔊 if you prefer silence). When the action dock
appears it starts locked while the background solver simulates; the controls
unlock the moment the depth badge turns green (about five seconds per
decision). Then either click a sizing chip and the red **Bet/Raise** button,
fine-tune the slider with the mouse wheel, or press **Fold** / **Call**
outright — every amount is in chips. Every decision is charted in the top bar; serious blunders
pause the table and render the played-vs-optimal breakdown in the **coach
feedback** panel to the right of the felt — press **Continue** and the coach
plays the best-EV action for you. Invalid clicks and reconnects are handled
gracefully: closing or refreshing the tab mid-hand saves the table, and the
next connection resumes that exact hand, street, and bet sizes from the
dashboard.

### Table events in the logs

Every meaningful table event is written to the console **and** to a rolling
log file at `data/app.log` (run with `RUST_LOG=info`, or `RUST_LOG=debug` for
inbound client frames too): hand deals (button, blinds, stacks, your hole
cards), each applied hero and opponent action (with street, pot, and acting
seat), every action submission as it is resolved or rejected (rejections log
the reason plus the full legal-action set, so a "stuck" click is traceable),
blunder interceptions (with the EV loss and threshold), review confirmations
(played blunder vs applied correction), and hand results. If something
misbehaves, send the relevant lines from `data/app.log` and the bug report
will land with full context.

When you're done with a table, click **Finish table** in the top bar — that
gives up: the tournament is stored as a LOSS at your current stack, you are
sent back to the dashboard, and a brand-new tournament becomes available.
(Closing the tab instead leaves the tournament open, ready to resume from the
dashboard.) The **Tournament history** link in the dashboard or top bar takes
you to <http://127.0.0.1:8744/tournaments>, where every
finished tournament shows the same action-EV graph as the live top bar in a
paginated list — 25 per page, newest first, with **← Newer** / **Older →**
navigation. Click
a tournament to open its detail page with the full stat breakdown. When a
tournament ends naturally (you bust out of chips, or one seat is left
standing), dealing stops, a
winner/loser modal
appears, and its **Continue** button jumps straight to that tournament's
detail page.

### Importing your GGPoker hand histories

PokerCraft exports your played tournaments as zip files. Put them in the
`history/` folder, then open the **Hand history** link (dashboard or table
header, <http://127.0.0.1:8744/history>) and press **Scan for new hand
histories**. The scan imports every hand and tournament found in the zips;
hands that were already imported are skipped, so you can re-scan whenever
new zips arrive. The results page shows the stats of only the hands imported
by that scan, and the history page keeps a running tournament listing (latest
first) with your total profit, win ratios, and per-tournament detail pages.

### Analyzing your opponents & training the bots against them

Right next to the scan button sits **Analyze imported opponents**. It replays
every opponent decision in your last 1,000 imported hands, solves each spot
from the opponent's perspective with the MCTS solver, and measures how many
big blinds the played action gave up against the best one. The pooled average
over both opponent seats becomes a single **field skill** (0 = leaks
5 BB/decision, 1 = solver-perfect); the analysis page shows the per-opponent
breakdown, the hands it skipped and why. Because a full pass is solver work,
the job runs in the background with a live progress counter — already-analyzed
hands are stored and skipped, so re-running only grades what is new. Press
**Use field skill … as the bot template** to save it: both local bots now pick
every decision through an MCTS solve of their own and then choose among the
candidate EVs with a skill-tempered spread — at skill 1 they always take the
best-EV action, and lower skills leak big blinds the way the analyzed field
did. The table header shows both sides on the same scale next to the EV graph:
**You 0.77 · Bots 0.48** compares your lifetime decision history against the
stored field template whenever a table is open. Clear the template from the
hand history page to restore the default placeholder opponents.

### Running the tests

```powershell
cargo test
```

Database integration tests need the local PostgreSQL instance running
(`.\pg.ps1`). They run against a separate `pokertrainer_test` database (created
by `pg.ps1`), so they never touch your real data.
