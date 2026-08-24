use crate::analytics::{ChartPoint, SessionSummary};
use crate::card::{Card, Suit};
use crate::decision::{Analysis, AnalyzedDecision, SearchReport};
use crate::game::{Action, GameState, Seat, Street};
use crate::opponent::OpponentSnapshot;
use crate::range::BetSize;
use crate::server::session::Sound;

/// The full app shell page: GGPoker-dark skin, top-bar lifetime EV chart, the
/// table controls (finish, tournament history, sound toggle), the table
/// column docked top-left, and the coach-feedback panel beside it (never
/// covering the table).
pub fn index_page() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer</title>
<link rel="stylesheet" href="/assets/style.css?v=10">
</head>
<body class="pt-body">
  <header class="pt-topwrap">
    <div class="pt-brand">Poker Trainer</div>
    <canvas id="ev-chart" width="1200" height="48" class="ev-chart"></canvas>
    <div id="ws-status" class="status-wait">connecting…</div>
    <div id="mcts-status" class="mcts-status status-bad">solver idle</div>
    <button id="sound-toggle" class="pt-icon-btn" type="button" title="Toggle table sounds">🔊</button>
    <a href="/tournaments" class="pt-link">Tournament history</a>
    <button id="finish-table" class="action-btn" type="button">Finish table</button>
  </header>
  <main class="pt-main">
    <div class="pt-layout">
      <section class="pt-table-col"><div id="table"></div></section>
      <aside class="pt-feedback-col">
        <h2 class="pt-feedback-heading">Coach feedback</h2>
        <div id="feedback">
          <div class="pt-feedback-empty">
            <b>No mistakes flagged yet.</b>
            When a decision costs enough equity, the played-vs-optimal breakdown
            appears here together with what the coach has learned about your
            opponents so far — and the table stays fully visible next to it.
          </div>
        </div>
      </aside>
    </div>
  </main>
  <script src="/assets/app.js?v=4"></script>
</body>
</html>"#
        .to_string()
}

/// The finished-tournament history page: one server-rendered card per
/// finished session whose decimated EV dataset is drawn client-side with the
/// same canvas style as the live top-bar chart.
pub fn tournaments_page(sessions: &[(SessionSummary, Vec<ChartPoint>)]) -> String {
    let mut html = String::from(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer — Tournaments</title>
<link rel="stylesheet" href="/assets/style.css?v=10">
</head>
<body class="pt-body">
<header class="pt-topwrap">
  <h1 class="pt-page-title">Tournaments</h1>
  <a href="/" class="pt-link">Back to the table</a>
</header>
<main class="pt-main">
"#,
    );

    if sessions.is_empty() {
        html.push_str(
            r#"<div class="pt-empty">No finished tournaments yet — play a table and finish it (or just close the tab) to see its EV history here.</div>"#,
        );
    } else {
        for (summary, points) in sessions {
            let dataset = serde_json::to_string(points).unwrap_or_else(|_| "[]".to_string());
            html.push_str(&format!(
                r#"<section class="pt-tournament" data-tournament-id="{}">
  <div class="pt-tournament-head">
    <span class="pt-tournament-title">Tournament #{}</span>
    <span class="pt-tournament-meta">{} → {}</span>
    <span class="pt-tournament-meta">{} hands · {} actions · avg EV loss {:.2} BB</span>
  </div>
  <canvas class="ev-chart" width="1200" height="48" data-points='{}'></canvas>
</section>"#,
                summary.id,
                summary.id,
                escape(&summary.started),
                escape(&summary.ended),
                summary.hands,
                summary.actions,
                summary.avg_ev_loss,
                dataset
            ));
        }
        html.push_str(
            r##"<script>
(() => {
  "use strict";
  document.querySelectorAll("canvas[data-points]").forEach((canvas) => {
    const ctx = canvas.getContext("2d");
    const values = JSON.parse(canvas.dataset.points || "[]").map((point) => point[1]);
    if (values.length < 2) return;
    const max = Math.max(1, ...values);
    const step = canvas.width / (values.length - 1);
    ctx.beginPath();
    values.forEach((value, i) => {
      const x = i * step;
      const y = canvas.height - (value / max) * (canvas.height - 6) - 3;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.strokeStyle = "#f59e0b";
    ctx.lineWidth = 2;
    ctx.stroke();
  });
})();
</script>"##,
        );
    }

    html.push_str("</main>\n</body>\n</html>\n");
    html
}

/// Escapes a dynamic string for safe HTML embedding.
fn escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A terse imperative label for an action, used on buttons and in the
/// tactical overlay: `Fold`, `Check`, `Call`, `Bet 100`, `Raise 400`, `All-in`.
pub fn action_label(action: Action) -> String {
    match action {
        Action::Fold => "Fold".to_string(),
        Action::Check => "Check".to_string(),
        Action::Call => "Call".to_string(),
        Action::Bet(amount) => format!("Bet {amount}"),
        Action::Raise(amount) => format!("Raise to {amount}"),
        Action::AllIn => "All-in".to_string(),
    }
}

/// A past-tense log line for an applied action, e.g. `You raise to 150`.
pub fn describe_action(seat: Seat, action: Action, call_amount: u32) -> String {
    let actor = match seat {
        Seat::Hero => "You".to_string(),
        other => other.to_string(),
    };
    match action {
        Action::Fold => format!("{actor} fold"),
        Action::Check => format!("{actor} check"),
        Action::Call => format!("{actor} call {call_amount}"),
        Action::Bet(amount) => format!("{actor} bet {amount}"),
        Action::Raise(amount) => format!("{actor} raise to {amount}"),
        Action::AllIn => format!("{actor} go all-in"),
    }
}

fn suit_symbol(suit: Suit) -> char {
    match suit {
        Suit::Clubs => '♣',
        Suit::Diamonds => '♦',
        Suit::Hearts => '♥',
        Suit::Spades => '♠',
    }
}

fn card_html(card: Card) -> String {
    let suit = card.suit();
    let suit_class = match suit {
        Suit::Hearts => " red",
        Suit::Diamonds => " blue",
        Suit::Clubs => " green",
        Suit::Spades => "",
    };
    let code = card.to_code();
    let rank = &code[..1];
    format!(
        r#"<span class="pt-card{suit_class}" data-suit="{:?}" data-code="{}"><b>{}</b><i>{}</i></span>"#,
        suit,
        escape(&code),
        escape(rank),
        suit_symbol(suit)
    )
}

fn sounds_json(sounds: &[Sound]) -> String {
    let tags: Vec<&str> = sounds.iter().map(|sound| sound.tag()).collect();
    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

/// Formats a stack pill: chips first, then the big-blind equivalent hidden
/// behind a `?` placeholder — holding Alt reveals the real value, so the
/// player learns to convert chips to blinds without the client doing it.
fn stack_text(stack: u32, big_blind: u32) -> String {
    let bb = stack as f32 / big_blind as f32;
    format!(
        "{stack}<span class=\"pt-bb\"><span class=\"pt-bb-q\">?</span><span class=\"pt-bb-real\" data-bb=\"{bb:.1}\">{bb:.1}&nbsp;BB</span></span>"
    )
}

/// The raw table-state HTML fragment swapped into the DOM on every state
/// change: a GGPoker-style oval felt with fixed seat positions (folded and
/// busted players stay seated), the board, the pot, the action dock in its
/// own right-aligned block below the oval (never covering the hero's cards),
/// and an always-visible action log docked to the left of the oval, exactly
/// as tall as the table. `sounds` carries the WebAudio cues the client
/// synthesizes for this update.
pub fn table_fragment(state: &GameState, hand_no: u64, log: &[String], sounds: &[Sound]) -> String {
    let level = state.blind_level();
    let mut html = String::new();

    html.push_str(&format!(
        r#"<div id="table-state" class="table-shell" data-hand="{hand_no}" data-sounds='{}'>"#,
        sounds_json(sounds)
    ));
    html.push_str(&format!(
        r#"<div class="pt-topbar"><span class="pt-pill">Hand #{hand_no}</span><span>Blinds {}/{} · {}</span><span class="pt-lvl-meta">Spin &amp; Gold · 3-Max</span></div>"#,
        level.small_blind,
        level.big_blind,
        escape(&state.street().to_string())
    ));

    html.push_str(r#"<div class="pt-table-body">"#);
    html.push_str(&action_log_panel(log));
    html.push_str(r#"<div class="pt-oval"><div class="pt-felt">"#);
    for seat in Seat::ALL {
        html.push_str(&seat_html(state, seat));
    }

    if state.total_pot() > 0 {
        html.push_str(&format!(
            r#"<div class="pt-pot">{}</div>"#,
            state.total_pot()
        ));
    }

    if !state.board().is_empty() {
        html.push_str(r#"<div class="pt-board">"#);
        for card in state.board() {
            html.push_str(&card_html(*card));
        }
        html.push_str("</div>");
    }

    if !state.is_hand_over() && state.to_act() != Seat::Hero {
        html.push_str(&format!(
            r#"<div class="pt-wait">Waiting for {}…</div>"#,
            escape(&state.to_act().to_string())
        ));
    }

    html.push_str("</div></div>");
    html.push_str("</div>");

    if !state.is_hand_over() && state.to_act() == Seat::Hero {
        html.push_str(r#"<div class="pt-action-block">"#);
        html.push_str(&action_panel(state));
        html.push_str("</div>");
    }

    html.push_str("</div>");
    html
}

/// The always-visible action log docked to the left of the oval, exactly as
/// tall as the table. Lines render top-to-bottom in chronological order — new
/// entries are inserted below older ones — and the client auto-scrolls the
/// panel so the newest line stays in view. Hand markers (`— Hand #N …`) get
/// gold emphasis so deals stand out between actions.
fn action_log_panel(log: &[String]) -> String {
    let mut html = String::from(
        r#"<aside class="pt-hlog"><div class="pt-hlog-title">Action log</div><div id="pt-hlog-lines" class="pt-hlog-lines">"#,
    );
    for line in log {
        let class = if line.starts_with('—') {
            "pt-hlog-line marker"
        } else {
            "pt-hlog-line"
        };
        html.push_str(&format!("<div class=\"{class}\">{}</div>", escape(line)));
    }
    html.push_str("</div></aside>");
    html
}

fn seat_html(state: &GameState, seat: Seat) -> String {
    let active = !state.is_hand_over() && state.to_act() == seat;
    let level = state.blind_level();
    let folded = state.folded(seat);
    let all_in = state.all_in(seat);
    let stack = state.stack(seat);
    let stack_pill = stack_text(stack, level.big_blind);

    let cards = match seat {
        Seat::Hero => format!(
            "{} {}",
            card_html(state.hero_cards()[0]),
            card_html(state.hero_cards()[1])
        ),
        _ => match state.hole_cards(seat) {
            Some(cards) => format!("{} {}", card_html(cards[0]), card_html(cards[1])),
            None => r#"<span class="pt-card back"></span><span class="pt-card back"></span>"#
                .to_string(),
        },
    };

    let small_blind = state.button();
    let big_blind = state.button().next();
    let mut badges = String::new();
    if state.button() == seat {
        badges.push_str(r#"<span class="pt-badge btn">BTN</span>"#);
    }
    if seat == small_blind {
        badges.push_str(r#"<span class="pt-badge sb">SB</span>"#);
    }
    if seat == big_blind {
        badges.push_str(r#"<span class="pt-badge bb">BB</span>"#);
    }

    let flag = if folded {
        r#"<span class="pt-flag">Fold</span>"#
    } else if all_in {
        r#"<span class="pt-flag allin">All-in</span>"#
    } else if stack == 0 {
        r#"<span class="pt-flag bust">Bust</span>"#
    } else {
        ""
    };

    let bet = state.street_contribution(seat);
    let bet_html = if bet > 0 {
        format!(r#"<div class="pt-bet">{bet}</div>"#)
    } else {
        String::new()
    };

    let win = state.hand_result().and_then(|result| {
        result
            .awards
            .iter()
            .find(|award| award.seat == seat)
            .map(|award| award.amount)
    });
    let win_html = match win {
        Some(amount) => format!(r#"<div class="pt-win"><b>WIN</b><span>+{amount}</span></div>"#),
        None => String::new(),
    };

    let cls = match (active, win.is_some()) {
        (true, _) => "pt-seat pt-active",
        (false, true) => "pt-seat pt-winner",
        (false, false) => "pt-seat",
    };
    let seat_name = escape(&seat.to_string());
    format!(
        r#"<div class="{cls}" data-seat="{seat_name}">
<div class="pt-seat-name">{seat_name}{badges}</div>
<div class="pt-seat-cards">{cards}{flag}</div>
<div class="pt-stack"><i class="pt-chip-dot"></i>{stack_pill}</div>
{bet_html}
{win_html}
</div>"#
    )
}

/// The GGPoker-style bottom dock overlaid on the felt: sizing chips (chip
/// values only — no BB labels), a golden bet slider with fine-grain wheel
/// control, and the Fold / Check-Call / Bet-Raise buttons.
fn action_panel(state: &GameState) -> String {
    let legal = state.legal_actions();
    let level = state.blind_level();
    let mut html = String::from(r#"<div id="action-panel" class="pt-action-dock">"#);

    let betting = legal.can_bet;
    let raising = legal.can_raise;
    let sizing = betting || raising;
    let kind = if betting {
        "bet"
    } else if raising {
        "raise"
    } else {
        ""
    };

    let mut initial = 0u32;
    if sizing {
        let to_call = if betting { 0 } else { legal.call_amount };
        let (min, max) = if betting {
            (legal.min_bet, legal.max_bet)
        } else {
            (legal.min_raise_to, legal.max_raise_to)
        };
        let stack = state.stack(Seat::Hero);

        html.push_str(r#"<div class="pt-bet-row">"#);
        let buckets: &[BetSize] = if state.street() == Street::Preflop {
            &[
                BetSize::Min,
                BetSize::ThreeBb,
                BetSize::FourBb,
                BetSize::Pot,
            ]
        } else {
            &[
                BetSize::Min,
                BetSize::HalfPot,
                BetSize::ThreeQuarterPot,
                BetSize::Pot,
            ]
        };
        let mut seen = Vec::new();
        for bucket in buckets {
            let raw = bucket.to_raise_to(state.total_pot(), to_call, level.big_blind, min, stack);
            let amount = raw.clamp(min, max);
            if amount >= stack || seen.contains(&amount) {
                continue;
            }
            seen.push(amount);
            html.push_str(&format!(
                r#"<button type="button" class="pt-chip-size" data-bucket="{}" data-size="{amount}">{amount}</button>"#,
                escape(bucket.label())
            ));
        }
        if legal.can_all_in {
            html.push_str(&format!(
                r#"<button type="button" class="pt-chip-size allin" data-bucket="ALLIN" data-size="{max}">All-in</button>"#
            ));
        }
        html.push_str("</div>");

        let preflop = state.street() == Street::Preflop;
        let default_bucket: BetSize = if preflop {
            BetSize::ThreeBb
        } else {
            BetSize::HalfPot
        };
        initial = default_bucket
            .to_raise_to(state.total_pot(), to_call, level.big_blind, min, stack)
            .clamp(min, max);

        html.push_str(&format!(
            r#"<div class="pt-slider-row">
<button type="button" class="pt-stepper" data-step="-1">−</button>
<input id="custom-amount" class="bet-slider" type="range" min="{min}" max="{max}" step="5" value="{initial}">
<button type="button" class="pt-stepper" data-step="1">+</button>
<input id="custom-amount-num" class="bet-number" type="number" min="{min}" max="{max}" value="{initial}">
</div>"#
        ));
    }

    html.push_str(r#"<div class="pt-action-row">"#);
    if legal.can_fold {
        html.push_str(
            r#"<button type="button" class="action-btn fold" data-kind="fold">Fold</button>"#,
        );
    }
    if legal.can_check {
        html.push_str(
            r#"<button type="button" class="action-btn green" data-kind="check">Check</button>"#,
        );
    }
    if legal.can_call {
        html.push_str(&format!(
            r#"<button type="button" class="action-btn green" data-kind="call">Call {}</button>"#,
            legal.call_amount
        ));
    }
    if !sizing && legal.can_all_in {
        html.push_str(
            r#"<button type="button" class="action-btn red" data-kind="all_in">All-in</button>"#,
        );
    }
    if sizing {
        let red_label = if betting {
            format!("Bet {initial}")
        } else {
            format!("Raise to {initial}")
        };
        html.push_str(&format!(
            r#"<button type="button" class="action-btn red" id="raise-btn" data-kind="{kind}">{red_label}</button>"#
        ));
    }
    html.push_str("</div>");

    html.push_str("</div>");
    html
}

/// The tactical-breakdown fragment rendered into the coach-feedback panel
/// beside the table: the opponents' live HUD cards first, then a plain-English
/// takeaway sentence, the played vs optimal action comparison, and the
/// candidate table sorted from cheapest (fold first) to all-in. Intercepted
/// blunders freeze the table: the card is titled accordingly and only offers
/// a confirmation that unlocks the transition (the coach's best-EV action).
pub fn tactical_overlay_fragment(
    hand_no: u64,
    decision: &AnalyzedDecision,
    intercepted: bool,
    opponents: &[OpponentSnapshot],
    big_blind: u32,
    call_amount: u32,
    hero_stack: u32,
) -> String {
    let optimal = decision.optimal;
    let mut html = String::from(r#"<div id="tactical-overlay" class="pt-tactical">"#);

    html.push_str(&opponents_block(opponents, big_blind));

    html.push_str(r#"<section class="pt-feedback-card">"#);
    html.push_str(r#"<div class="pt-overlay-card">"#);
    if intercepted {
        html.push_str(&format!(
            r#"<h2 class="pt-overlay-title">Hand #{hand_no} — Blunder intercepted</h2>"#
        ));
    } else {
        html.push_str(&format!(
            r#"<h2 class="pt-overlay-title">Hand #{hand_no} — Decision review</h2>"#
        ));
    }

    html.push_str(&ev_diff_sentence(decision));

    if let Some(played) = &decision.played {
        html.push_str(&format!(
            r#"<div class="pt-compare"><div class="pt-played">You played <b>{}</b> — EV {:.1}</div><div class="pt-optimal">Optimal: <b>{}</b> — EV {:.1}</div></div>"#,
            escape(&action_label(played.analysis.action)),
            played.analysis.ev,
            escape(&action_label(optimal.action)),
            optimal.ev
        ));
        html.push_str(&format!(
            r#"<div class="pt-ev-loss">EV lost: <b>{:.2}</b> BB</div>"#,
            played.ev_loss_bb
        ));
    } else {
        html.push_str(&format!(
            r#"<div class="pt-compare"><div class="pt-optimal">Optimal: <b>{}</b> — EV {:.1}</div></div>"#,
            escape(&action_label(optimal.action)),
            optimal.ev
        ));
    }

    html.push_str(
        r#"<table class="pt-ranking"><tr><th>Action</th><th>EV</th><th>σ</th><th>Bust</th><th>Visits</th></tr>"#,
    );
    let mut rows: Vec<&Analysis> = decision.ranking.iter().collect();
    rows.sort_by_key(|analysis| chip_cost(analysis.action, call_amount, hero_stack));
    for analysis in rows {
        let row_class = if analysis.action == optimal.action {
            "optimal"
        } else if decision
            .played
            .as_ref()
            .is_some_and(|played| played.analysis.action == analysis.action)
        {
            "played"
        } else {
            ""
        };
        html.push_str(&format!(
            r#"<tr class="{row_class}"><td>{}</td><td>{:.1}</td><td>{:.0}</td><td>{:.1}%</td><td>{}</td></tr>"#,
            escape(&action_label(analysis.action)),
            analysis.ev,
            analysis.sigma(),
            analysis.bust_prob * 100.0,
            analysis.visits
        ));
    }
    html.push_str("</table>");

    html.push_str(&search_effort_html(&decision.search));

    if intercepted {
        html.push_str(
            r#"<button class="action-btn pt-confirm" data-overlay-confirm>Continue</button>"#,
        );
    } else {
        html.push_str(r#"<button class="action-btn" data-overlay-close>Continue</button>"#);
    }
    html.push_str("</div></section>");
    html.push_str("</div>");
    html
}

/// The chips an action commits on the current street — the sort key for the
/// candidate table, which always reads cheapest-first (fold leads everything
/// else when it is an action, all-in closes the list).
fn chip_cost(action: Action, call_amount: u32, hero_stack: u32) -> u32 {
    match action {
        Action::Fold | Action::Check => 0,
        Action::Call => call_amount,
        Action::Bet(amount) | Action::Raise(amount) => amount,
        Action::AllIn => hero_stack,
    }
}

/// A plain-language takeaway shown above the raw EV numbers: names the
/// played and optimal actions and translates the EV gap into what it means
/// for a human stack. Bigger leaks get sharper language.
fn ev_diff_sentence(decision: &AnalyzedDecision) -> String {
    let optimal_label = action_label(decision.optimal.action);
    let sentence = match decision.played.as_ref() {
        Some(played) if played.is_optimal => {
            format!("Perfect — {optimal_label} was the highest-value line and you took it.")
        }
        Some(played) => {
            let played_label = action_label(played.analysis.action);
            let loss = played.ev_loss_bb;
            if loss < 0.5 {
                format!(
                    "A rounding error: {played_label} instead of {optimal_label} costs only {loss:.2} BB in the long run — nothing to sweat."
                )
            } else if loss < 2.0 {
                format!(
                    "That one adds up: {played_label} gives up about {loss:.2} BB versus {optimal_label} every time this spot repeats."
                )
            } else if loss < 5.0 {
                format!(
                    "Costly: {played_label} sacrifices roughly {loss:.2} BB compared with {optimal_label} — you will feel this over a session."
                )
            } else {
                format!(
                    "A big leak: {played_label} burns about {loss:.2} BB relative to {optimal_label}. Mistakes this size turn green sessions red."
                )
            }
        }
        None => format!("The highest-value line in this spot is {optimal_label}."),
    };
    format!(r#"<div class="pt-ev-diff">{}</div>"#, escape(&sentence))
}

/// The opponents' HUD cards: seat name with position badges and live status,
/// the stack pill (chips with the `?`/Alt BB reveal), the stat grid, and the
/// player-friendly read of each opponent's play.
fn opponents_block(opponents: &[OpponentSnapshot], big_blind: u32) -> String {
    let mut html = String::from(
        r#"<section class="pt-feedback-card pt-opp-block"><h2 class="pt-overlay-title">Opponents</h2><div class="pt-opp-grid">"#,
    );
    for opponent in opponents {
        let mut badges = String::new();
        if opponent.is_button {
            badges.push_str(r#"<span class="pt-badge btn">BTN</span>"#);
        }
        if opponent.is_small_blind {
            badges.push_str(r#"<span class="pt-badge sb">SB</span>"#);
        }
        if opponent.is_big_blind {
            badges.push_str(r#"<span class="pt-badge bb">BB</span>"#);
        }
        let flag = if opponent.folded {
            r#"<span class="pt-opp-flag fold">Folded</span>"#
        } else if opponent.all_in {
            r#"<span class="pt-opp-flag allin">All-in</span>"#
        } else if opponent.stack == 0 {
            r#"<span class="pt-opp-flag bust">Busted</span>"#
        } else {
            ""
        };

        let aggression = match (opponent.postflop_bets, opponent.postflop_calls) {
            (0, 0) => "—".to_string(),
            (_, 0) => "∞".to_string(),
            (bets, calls) => format!("{:.1}", bets as f64 / calls as f64),
        };

        html.push_str(&format!(
            r#"<article class="pt-opp-card">
<div class="pt-opp-head"><span class="pt-opp-name">{}{}</span>{}<span class="pt-stack">{}</span></div>
<div class="pt-opp-stats">
<div class="pt-opp-stat"><span>Hands</span><b>{}</b></div>
<div class="pt-opp-stat"><span>VPIP</span><b>{:.0}%</b></div>
<div class="pt-opp-stat"><span>PFR</span><b>{:.0}%</b></div>
<div class="pt-opp-stat"><span>Folds to bet</span><b>{:.0}%</b></div>
<div class="pt-opp-stat"><span>Aggression</span><b>{}</b></div>
</div>
<p class="pt-opp-read">{}</p>
</article>"#,
            escape(&opponent.seat.to_string()),
            badges,
            flag,
            stack_text(opponent.stack, big_blind),
            opponent.hands,
            opponent.vpip_pct,
            opponent.pfr_pct,
            opponent.fold_to_bet_pct,
            aggression,
            escape(&opponent.read),
        ));
    }
    html.push_str("</div></section>");
    html
}

/// A plain-language summary of the search effort behind a decision: a color
/// grade (how much work the coach did), a caption in everyday words, and a
/// confidence note. The raw numbers stay available in the tooltip.
struct SearchEffort {
    class: &'static str,
    label: &'static str,
    caption: String,
    note: &'static str,
    tooltip: String,
}

fn search_effort(search: &SearchReport) -> SearchEffort {
    let root_visits = search.worlds * search.iterations;
    let (class, label, note) = if root_visits >= 10_000 {
        (
            "pt-search-deep",
            "Deep search",
            "Extra thorough — high confidence.",
        )
    } else if root_visits >= 3_000 {
        (
            "pt-search-solid",
            "Solid search",
            "Thorough enough for standard decisions.",
        )
    } else {
        (
            "pt-search-quick",
            "Quick search",
            "A fast read — fine for straightforward spots.",
        )
    };

    let depth = if search.max_tree_depth == search.max_depth {
        format!("thinking up to {} moves ahead", search.max_tree_depth)
    } else {
        format!(
            "thinking up to {} move{} of a planned {}",
            search.max_tree_depth,
            if search.max_tree_depth == 1 { "" } else { "s" },
            search.max_depth
        )
    };
    let caption = format!(
        "Played out {} possible opponent hands × {} evaluations each, {depth} — {} simulated actions.",
        search.worlds,
        search.iterations,
        human_count(search.rollout_actions)
    );
    let tooltip = format!(
        "worlds = {} · iterations = {} · tree depth {}/{} · nodes = {} · rollout actions = {}",
        search.worlds,
        search.iterations,
        search.max_tree_depth,
        search.max_depth,
        search.nodes,
        search.rollout_actions
    );

    SearchEffort {
        class,
        label,
        caption,
        note,
        tooltip,
    }
}

/// Humanizes a big count: `1,240 → 1.2k`, `13,838 → 13.8k`, small counts
/// unchanged.
fn human_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn search_effort_html(search: &SearchReport) -> String {
    let effort = search_effort(search);
    format!(
        r#"<div class="pt-search-meta {}" title="{}">
<span class="pt-search-badge">{}</span>
<span class="pt-search-body"><span class="pt-search-caption">{}</span><span class="pt-search-note">{}</span></span>
</div>"#,
        effort.class, effort.tooltip, effort.label, effort.caption, effort.note
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Deck;
    use crate::card::Rank;
    use crate::decision::{Analysis, PlayedEvaluation, SearchReport};
    use crate::game::blinds::BlindLevel;
    use crate::rng::seeded_rng;

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    #[test]
    fn action_labels_cover_every_variant() {
        assert_eq!(action_label(Action::Fold), "Fold");
        assert_eq!(action_label(Action::Check), "Check");
        assert_eq!(action_label(Action::Call), "Call");
        assert_eq!(action_label(Action::Bet(100)), "Bet 100");
        assert_eq!(action_label(Action::Raise(400)), "Raise to 400");
        assert_eq!(action_label(Action::AllIn), "All-in");
    }

    #[test]
    fn describe_action_uses_plain_english() {
        assert_eq!(
            describe_action(Seat::Hero, Action::Raise(60), 20),
            "You raise to 60"
        );
        assert_eq!(
            describe_action(Seat::Opponent2, Action::Call, 20),
            "Opponent 2 call 20"
        );
        assert_eq!(
            describe_action(Seat::Opponent1, Action::AllIn, 100),
            "Opponent 1 go all-in"
        );
    }

    #[test]
    fn escape_neutralizes_html_metacharacters() {
        assert_eq!(
            escape(r#"<script>"a&b"</script>"#),
            "&lt;script&gt;&quot;a&amp;b&quot;&lt;/script&gt;"
        );
    }

    #[test]
    fn index_page_shell_points_at_the_ws_client() {
        let page = index_page();
        assert!(page.contains("<title>Poker Trainer</title>"));
        assert!(page.contains(r#"<div id="table"></div>"#));
        assert!(page.contains(r#"<div id="feedback">"#));
        assert!(page.contains(r#"/assets/app.js"#));
        assert!(
            page.contains(r#"id="finish-table""#),
            "the finish control is present"
        );
        assert!(
            page.contains(r#"href="/tournaments""#),
            "the tournament history link is present"
        );
        assert!(
            page.contains(r#"id="sound-toggle""#),
            "the sound toggle is present"
        );
        assert!(
            page.contains(r#"id="mcts-status""#),
            "the solver depth badge is present"
        );
        assert!(
            page.contains(r#"/assets/style.css?v=10"#),
            "the stylesheet link is versioned so browsers drop stale cached CSS"
        );
        assert!(
            !page.contains("cdn.tailwindcss.com"),
            "the skin ships its own CSS and works offline"
        );
    }

    fn summary(
        id: i32,
        started: &str,
        ended: &str,
        actions: i64,
        hands: i32,
        avg_ev_loss: f64,
    ) -> SessionSummary {
        SessionSummary {
            id,
            started: started.to_string(),
            ended: ended.to_string(),
            actions,
            hands,
            avg_ev_loss,
        }
    }

    #[test]
    fn tournaments_page_has_an_empty_state() {
        let empty: Vec<(SessionSummary, Vec<ChartPoint>)> = Vec::new();
        let page = tournaments_page(&empty);
        assert!(page.contains("<title>Poker Trainer — Tournaments</title>"));
        assert!(page.contains("No finished tournaments yet"));
        assert!(
            !page.contains("data-tournament-id"),
            "no cards without finished sessions"
        );
    }

    #[test]
    fn tournaments_page_renders_one_card_per_session_with_chart_data() {
        let sessions = vec![
            (
                summary(
                    7,
                    "2026-08-01T10:00:00Z",
                    "2026-08-01T10:05:00Z",
                    3,
                    3,
                    12.5,
                ),
                vec![(1, 0.0), (2, 30.0), (3, 12.5)],
            ),
            (
                summary(
                    42,
                    "2026-08-02T09:00:00Z",
                    "2026-08-02T09:07:00Z",
                    5,
                    2,
                    2.25,
                ),
                vec![(1, 4.5), (2, 0.0)],
            ),
        ];
        let page = tournaments_page(&sessions);
        assert!(page.contains(r#"data-tournament-id="7""#));
        assert!(page.contains("Tournament #7"));
        assert!(page.contains("3 hands · 3 actions · avg EV loss 12.50 BB"));
        assert!(page.contains("2026-08-01T10:00:00Z → 2026-08-01T10:05:00Z"));
        assert!(page.contains("Tournament #42"));
        assert!(
            page.contains(r#"data-points='[[1,0.0],[2,30.0],[3,12.5]]'"#),
            "decimated datasets are embedded for the client chart"
        );
        assert!(page.contains(r#"data-points='[[1,4.5],[2,0.0]]'"#));
        assert!(page.contains("canvas[data-points]"));
    }

    #[test]
    fn tournaments_page_escapes_database_strings() {
        let sessions = vec![(
            summary(1, r#"<script>"evil"</script>"#, "end", 1, 1, 0.0),
            vec![(1, 0.0)],
        )];
        let page = tournaments_page(&sessions);
        assert!(!page.contains(r#"<script>"evil""#));
        assert!(page.contains("&lt;script&gt;"));
    }

    #[test]
    fn table_fragment_reflects_the_current_state() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(31)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let fragment = table_fragment(&state, 3, &["You check".to_string()], &[]);
        assert!(fragment.contains(r#"id="table-state""#));
        assert!(fragment.contains("Hand #3"));
        assert!(fragment.contains("Blinds 10/20 · Preflop"));
        assert!(
            fragment.contains(r#"data-kind="call">Call 10"#),
            "{fragment}"
        );
        assert!(fragment.contains(r#"data-kind="fold"#));
        assert!(fragment.contains("You check"), "log lines are shown");
        let hero_cards = state.hero_cards();
        for card in hero_cards {
            assert!(
                fragment.contains(&format!(r#"data-code="{}""#, card)),
                "hero cards are visible: {fragment}"
            );
        }
        assert!(
            fragment.contains(r#"class="pt-card back"#),
            "opponent cards stay hidden"
        );
        assert!(
            fragment.contains(r#"<span class="pt-bb-q">?</span>"#),
            "stacks show a `?` placeholder instead of the BB equivalent: {fragment}"
        );
        assert!(
            fragment.contains(r#"class="pt-bb-real" data-bb="#),
            "the BB equivalent is embedded for the Alt-hold reveal: {fragment}"
        );
    }

    #[test]
    fn table_fragment_folds_keep_their_seat_position() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(32)))
            .unwrap();
        state.apply_action(Action::Fold).unwrap();
        let fragment = table_fragment(&state, 1, &[], &[]);
        assert!(
            fragment.contains(r#"class="pt-seat pt-active" data-seat="Hero""#)
                || fragment.contains(r#"class="pt-seat" data-seat="Opponent 2""#),
            "the table renders every seat regardless of folds: {fragment}"
        );
        assert!(
            fragment.contains(r#"class="pt-flag">Fold"#),
            "the folded seat is flagged in place: {fragment}"
        );
        assert!(
            fragment.matches(r#"data-seat="Hero""#).count() >= 1,
            "folded players are never re-seated"
        );
    }

    #[test]
    fn action_panel_labels_amounts_in_chips_not_blinds() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(33)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        let fragment = table_fragment(&state, 1, &[], &[]);
        assert!(
            !fragment.contains(">3BB<")
                && !fragment.contains(">4BB<")
                && !fragment.contains(">2BB<"),
            "sizing chips show chip amounts, never BB labels: {fragment}"
        );
        assert!(
            fragment.contains(r#"data-bucket="3BB""#),
            "bucket identity stays on the wire protocol: {fragment}"
        );
        assert!(fragment.contains(r#"data-kind="raise""#));
        assert!(
            fragment.contains(r#"data-bucket="ALLIN" data-size="500""#),
            "the all-in chip is a live preset that drives the slider to the whole stack: {fragment}"
        );
    }

    #[test]
    fn action_panel_offers_a_direct_all_in_when_calling_costs_the_whole_stack() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(36)))
            .unwrap();
        state.apply_action(Action::Raise(500)).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let fragment = table_fragment(&state, 1, &[], &[]);
        assert!(
            fragment.contains(r#"data-kind="all_in""#),
            "a hero who can only call for the whole stack still gets an all-in button: {fragment}"
        );
        assert!(
            !fragment.contains(r#"data-bucket="ALLIN""#),
            "the sizing dock is hidden when raising is impossible: {fragment}"
        );
    }

    #[test]
    fn table_fragment_carries_sound_cues() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(34)))
            .unwrap();
        let fragment = table_fragment(
            &state,
            1,
            &[],
            &[Sound::Deal, Sound::Chip, Sound::Fold, Sound::Win],
        );
        assert!(
            fragment.contains(r#"data-sounds='["deal","chip","fold","win"]'"#),
            "{fragment}"
        );
    }

    #[test]
    fn table_fragment_shows_waiting_and_the_win_badge_when_appropriate() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(35)))
            .unwrap();
        assert_ne!(state.to_act(), Seat::Hero);
        let waiting = table_fragment(&state, 1, &[], &[]);
        assert!(waiting.contains("Waiting for"));

        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        let finished = table_fragment(&state, 1, &[], &[]);
        assert!(
            finished.contains(r#"class="pt-win"><b>WIN</b><span>+30</span>"#),
            "the win is shown next to the winner, not in the centre: {finished}"
        );
        assert!(
            finished.contains(r#"data-seat="Opponent 1" class="pt-seat pt-winner""#)
                || finished.contains(r#"class="pt-seat pt-winner" data-seat="Opponent 1""#),
            "the winner's seat is marked: {finished}"
        );
        assert!(
            !finished.contains("pt-result"),
            "the centre result banner is gone: {finished}"
        );
        assert!(!finished.contains(r#"id="action-panel""#));
        assert!(
            !finished.contains(r#"class="pt-wait""#),
            "no waiting pill once the hand is over: {finished}"
        );
    }

    #[test]
    fn cards_render_with_four_deck_colors() {
        for (rank, suit, class) in [
            (Rank::Ace, Suit::Hearts, "pt-card red"),
            (Rank::King, Suit::Diamonds, "pt-card blue"),
            (Rank::Queen, Suit::Clubs, "pt-card green"),
            (Rank::Jack, Suit::Spades, "pt-card"),
        ] {
            let html = card_html(Card::new(rank, suit));
            assert!(
                html.starts_with(&format!(r#"<span class="{class}""#)),
                "suit {suit:?} maps to `{class}`: {html}"
            );
        }
    }

    #[test]
    fn action_dock_sits_below_the_oval_never_over_the_cards() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(38)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let fragment = table_fragment(&state, 2, &[], &[]);
        let oval_marker = fragment.find(r#"<div class="pt-oval""#).unwrap();
        let dock = fragment.find(r#"id="action-panel""#).unwrap();
        assert!(
            dock > oval_marker,
            "the action panel renders in its own block below the oval: {fragment}"
        );
        assert!(
            fragment.contains(r#"<div class="pt-action-block"><div id="action-panel""#),
            "{fragment}"
        );
    }

    #[test]
    fn seats_render_without_avatar_icons() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(39)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        let fragment = table_fragment(&state, 4, &[], &[]);
        assert!(
            !fragment.contains(r#"class="pt-avatar""#),
            "no round avatar icons for any seat: {fragment}"
        );
        assert!(fragment.contains(r#"data-seat="Hero""#));
        assert!(fragment.contains(r#"data-seat="Opponent 1""#));
        assert!(
            fragment.contains(r#"<div class="pt-seat-cards">"#),
            "cards are still rendered: {fragment}"
        );
    }

    #[test]
    fn action_log_docks_left_of_the_table_with_newest_lines_below() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(40)))
            .unwrap();
        let log = vec![
            "— Hand #1 — blinds 10/20".to_string(),
            "Opponent 2 call 20".to_string(),
            "You raise to 60".to_string(),
        ];

        let fragment = table_fragment(&state, 1, &log, &[]);
        let panel = fragment.find(r#"class="pt-hlog""#).unwrap();
        let oval = fragment.find(r#"<div class="pt-oval""#).unwrap();
        assert!(
            panel < oval,
            "the action log docks left of the oval: {fragment}"
        );
        assert!(fragment.contains(r#"id="pt-hlog-lines""#));
        let dealt = fragment.find("— Hand #1 — blinds 10/20").unwrap();
        let latest = fragment.find("You raise to 60").unwrap();
        assert!(
            latest > dealt,
            "newer lines render below older ones: {fragment}"
        );
        assert!(
            fragment.contains(r#"class="pt-hlog-line marker">— Hand #1"#),
            "hand markers get gold emphasis: {fragment}"
        );
        assert!(
            !fragment.contains("pt-log-toggle"),
            "the collapsible history toggle is gone: {fragment}"
        );
    }

    #[test]
    fn street_bets_render_in_front_of_every_seat_until_round_ends() {
        let mut state = GameState::new(Seat::Hero, level());
        let mut deck = Deck::shuffled(&mut seeded_rng(41));
        state.start_hand(&mut deck).unwrap();

        // Blinds count as street bets: the button (hero) posted 10, the BB 20.
        let fragment = table_fragment(&state, 5, &[], &[]);
        assert!(
            fragment.contains(r#"class="pt-bet">10</div>"#),
            "the small blind shows in front of the hero: {fragment}"
        );
        assert!(
            fragment.contains(r#"class="pt-bet">20</div>"#),
            "the big blind shows in front of its seat: {fragment}"
        );

        // Opponent 2 raises to 60: their street bet badge reads 60.
        state.apply_action(Action::Raise(60)).unwrap();
        let raised = table_fragment(&state, 5, &[], &[]);
        assert!(
            raised.contains(r#"class="pt-bet">60</div>"#),
            "the raise amount shows in front of the raiser: {raised}"
        );

        // Close the betting round: street bets are swept into the pot pill.
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.advance_street(&mut deck).unwrap();
        let settled = table_fragment(&state, 5, &[], &[]);
        assert!(
            !settled.contains("pt-bet"),
            "street bets are gone once the round closes: {settled}"
        );
        assert!(
            settled.contains(r#"class="pt-pot">180</div>"#),
            "the pot pill carries the whole round: {settled}"
        );
    }

    fn sample_analysis() -> AnalyzedDecision {
        let fold = Analysis {
            action: Action::Fold,
            bucket: None,
            ev: 0.0,
            variance: 0.0,
            bust_prob: 0.0,
            score: 0.0,
            visits: 120,
        };
        let call = Analysis {
            action: Action::Call,
            bucket: None,
            ev: -18.0,
            variance: 2000.0,
            bust_prob: 0.1,
            score: -25.0,
            visits: 80,
        };
        AnalyzedDecision {
            ranking: vec![fold, call],
            optimal: fold,
            played: Some(PlayedEvaluation {
                analysis: call,
                ev_loss_bb: 0.9,
                is_optimal: false,
            }),
            search: SearchReport {
                worlds: 16,
                iterations: 96,
                max_depth: 3,
                max_tree_depth: 3,
                nodes: 1240,
                rollout_actions: 4120,
            },
        }
    }

    #[test]
    fn tactical_overlay_compares_played_and_optimal() {
        let fragment = tactical_overlay_fragment(
            7,
            &sample_analysis(),
            false,
            &sample_opponents(),
            20,
            20,
            500,
        );
        assert!(fragment.contains("Hand #7 — Decision review"));
        assert!(fragment.contains(r#"class="pt-ev-diff""#));
        assert!(
            fragment.contains("That one adds up: Call gives up about 0.90 BB versus Fold"),
            "{fragment}"
        );
        assert!(fragment.contains("You played <b>Call</b>"));
        assert!(fragment.contains("Optimal: <b>Fold</b>"));
        assert!(fragment.contains("EV lost: <b>0.90</b> BB"));
        assert!(fragment.contains(r#"<tr class="optimal"><td>Fold</td>"#));
        assert!(fragment.contains(r#"<tr class="played"><td>Call</td>"#));
        assert!(fragment.contains("<th>Visits</th>"));
        assert!(fragment.contains(r#"data-overlay-close"#));
        assert!(
            fragment.contains("Quick search"),
            "the friendly search-effort line replaces the raw jargon: {fragment}"
        );
        assert!(
            fragment.contains(
                "Played out 16 possible opponent hands × 96 evaluations each, thinking up to 3 moves ahead — 4.1k simulated actions."
            ),
            "{fragment}"
        );
        assert!(
            fragment.contains(r#"title="worlds = 16 · iterations = 96"#),
            "the raw numbers stay reachable in the tooltip: {fragment}"
        );
        assert!(
            fragment.contains(r#"class="pt-feedback-card""#),
            "the breakdown renders in the coach panel beside the table"
        );
    }

    #[test]
    fn intercepted_overlay_is_titled_flagged_and_only_confirms() {
        let fragment = tactical_overlay_fragment(
            7,
            &sample_analysis(),
            true,
            &sample_opponents(),
            20,
            20,
            500,
        );
        assert!(fragment.contains("Hand #7 — Blunder intercepted"));
        assert!(
            !fragment.contains("The table is paused"),
            "the pause note is gone — the title carries the message: {fragment}"
        );
        assert!(fragment.contains(r#"data-overlay-confirm"#));
        assert!(fragment.contains("Continue"));
        assert!(
            !fragment.contains(r#"data-overlay-close"#),
            "an intercepted modal cannot be silently dismissed"
        );
    }

    #[test]
    fn tactical_overlay_handles_a_missing_played_action() {
        let mut decision = sample_analysis();
        decision.played = None;
        let fragment = tactical_overlay_fragment(7, &decision, false, &[], 20, 20, 500);
        assert!(!fragment.contains("EV lost"));
        assert!(fragment.contains("Optimal: <b>Fold</b>"));
        assert!(
            fragment.contains("The highest-value line in this spot is Fold."),
            "{fragment}"
        );
    }

    #[test]
    fn candidate_table_sorts_cheapest_first_with_fold_on_top() {
        let mut decision = sample_analysis();
        // Survivability order is deliberate noise: the display must re-sort
        // by chips committed — fold (0), check (0), call (20), raise (160),
        // all-in (500).
        decision.ranking = vec![
            Analysis {
                ev: 900.0,
                score: 9.0,
                ..decision.ranking[0]
            },
            Analysis {
                action: Action::AllIn,
                ev: 200.0,
                score: 8.0,
                ..decision.ranking[0]
            },
            Analysis {
                action: Action::Raise(160),
                ev: 150.0,
                score: 7.0,
                ..decision.ranking[0]
            },
            Analysis {
                action: Action::Check,
                ev: 0.0,
                score: 6.0,
                ..decision.ranking[0]
            },
            Analysis {
                action: Action::Call,
                ev: -20.0,
                score: 5.0,
                ..decision.ranking[0]
            },
        ];
        decision.optimal = decision.ranking[0];
        decision.played = None;
        let fragment = tactical_overlay_fragment(7, &decision, false, &[], 20, 20, 500);
        let fold = fragment.find("<td>Fold</td>").unwrap();
        let check = fragment.find("<td>Check</td>").unwrap();
        let call = fragment.find("<td>Call</td>").unwrap();
        let raise = fragment.find("<td>Raise to 160</td>").unwrap();
        let all_in = fragment.find("<td>All-in</td>").unwrap();
        assert!(fold < check, "fold leads the table: {fragment}");
        assert!(check < call);
        assert!(call < raise);
        assert!(
            raise < all_in,
            "losing chips sorts before all-in: {fragment}"
        );
    }

    #[test]
    fn chip_cost_orders_fold_check_call_bets_and_all_in() {
        assert_eq!(chip_cost(Action::Fold, 20, 500), 0);
        assert_eq!(chip_cost(Action::Check, 20, 500), 0);
        assert_eq!(chip_cost(Action::Call, 20, 500), 20);
        assert_eq!(chip_cost(Action::Bet(60), 20, 500), 60);
        assert_eq!(chip_cost(Action::Raise(240), 20, 500), 240);
        assert_eq!(chip_cost(Action::AllIn, 20, 500), 500);
    }

    #[test]
    fn ev_diff_sentence_gets_sharper_as_losses_grow() {
        let base = sample_analysis();
        let played_ev_loss = |ev_loss_bb| {
            let mut decision = base.clone();
            decision
                .played
                .as_mut()
                .expect("sample has a played evaluation")
                .ev_loss_bb = ev_loss_bb;
            decision
        };

        assert!(ev_diff_sentence(&played_ev_loss(0.25)).contains("rounding error"));
        assert!(ev_diff_sentence(&played_ev_loss(0.9)).contains("adds up"));
        assert!(ev_diff_sentence(&played_ev_loss(3.0)).contains("Costly"));
        assert!(ev_diff_sentence(&played_ev_loss(6.5)).contains("big leak"));

        let mut perfect = base.clone();
        perfect
            .played
            .as_mut()
            .expect("sample has a played evaluation")
            .is_optimal = true;
        assert!(ev_diff_sentence(&perfect).contains("you took it"));
    }

    #[test]
    fn opponents_block_renders_stats_statuses_and_reads() {
        let fragment = opponents_block(&sample_opponents(), 20);
        assert!(fragment.contains("Opponent 1"));
        assert!(fragment.contains(r#"<span class="pt-badge btn">BTN</span>"#));
        assert!(fragment.contains(r#"class="pt-opp-flag allin">All-in</span>"#));
        assert!(fragment.contains("<span>VPIP</span><b>67%</b>"));
        assert!(fragment.contains("<span>PFR</span><b>33%</b>"));
        assert!(fragment.contains("<span>Folds to bet</span><b>25%</b>"));
        assert!(fragment.contains("<span>Aggression</span><b>1.5</b>"));
        assert!(fragment.contains("Loose aggressive — in lots of pots and swinging."));
        assert!(fragment.contains("No hands played yet."), "{fragment}");
    }

    #[test]
    fn opponents_block_marks_endless_aggression_as_infinite() {
        let mut opponents = sample_opponents();
        opponents[1] = OpponentSnapshot {
            postflop_bets: 4,
            postflop_calls: 0,
            ..opponents[1].clone()
        };
        let fragment = opponents_block(&opponents, 20);
        assert!(fragment.contains("<span>Aggression</span><b>∞</b>"));
    }

    fn sample_opponents() -> Vec<OpponentSnapshot> {
        vec![
            OpponentSnapshot {
                seat: Seat::Opponent1,
                hands: 12,
                vpip_pct: 66.7,
                pfr_pct: 33.3,
                fold_to_bet_pct: 25.0,
                postflop_bets: 6,
                postflop_calls: 4,
                read: "Loose aggressive — in lots of pots and swinging.".to_string(),
                stack: 640,
                folded: false,
                all_in: true,
                is_button: true,
                is_small_blind: true,
                is_big_blind: false,
            },
            OpponentSnapshot {
                seat: Seat::Opponent2,
                hands: 0,
                vpip_pct: 0.0,
                pfr_pct: 0.0,
                fold_to_bet_pct: 0.0,
                postflop_bets: 0,
                postflop_calls: 0,
                read: "No hands played yet.".to_string(),
                stack: 0,
                folded: false,
                all_in: false,
                is_button: false,
                is_small_blind: false,
                is_big_blind: false,
            },
        ]
    }

    #[test]
    fn human_count_scales_and_keeps_small_counts() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(25), "25");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1_240), "1.2k");
        assert_eq!(human_count(9_999), "10.0k");
        assert_eq!(human_count(10_000), "10.0k");
        assert_eq!(human_count(13_838), "13.8k");
        assert_eq!(human_count(120_000), "120.0k");
        assert_eq!(human_count(1_000_000), "1.0M");
        assert_eq!(human_count(2_300_000), "2.3M");
    }

    #[test]
    fn search_effort_grades_by_root_visits() {
        assert_eq!(search_effort(&search(32, 62)).class, "pt-search-quick");
        assert_eq!(search_effort(&search(16, 192)).class, "pt-search-solid");
        assert_eq!(search_effort(&search(128, 80)).class, "pt-search-deep");
    }

    /// A baseline report with the uncommon fields fixed; `worlds` and
    /// `iterations` parameterize the root-visit grade.
    fn search(worlds: usize, iterations: usize) -> SearchReport {
        SearchReport {
            worlds,
            iterations,
            max_depth: 3,
            max_tree_depth: 3,
            nodes: 100,
            rollout_actions: 200,
        }
    }

    #[test]
    fn search_effort_mentions_a_fallen_short_depth() {
        let effort = search_effort(&SearchReport {
            worlds: 8,
            iterations: 200,
            max_depth: 4,
            max_tree_depth: 1,
            nodes: 100,
            rollout_actions: 200,
        });
        assert_eq!(
            effort.caption,
            "Played out 8 possible opponent hands × 200 evaluations each, thinking up to 1 move of a planned 4 — 200 simulated actions."
        );
    }

    #[test]
    fn search_effort_reached_cap_reads_in_moves() {
        let effort = search_effort(&SearchReport {
            worlds: 8,
            iterations: 200,
            max_depth: 3,
            max_tree_depth: 3,
            nodes: 100,
            rollout_actions: 12_345,
        });
        assert!(effort.caption.contains("thinking up to 3 moves ahead"));
        assert!(effort.caption.contains("12.3k simulated actions"));
    }

    #[test]
    fn showdown_fragment_reveals_cards_and_marks_winners() {
        let mut state = GameState::new(Seat::Hero, level());
        let mut deck = Deck::shuffled(&mut seeded_rng(36));
        state.start_hand(&mut deck).unwrap();
        state.apply_action(Action::AllIn).unwrap();
        state.apply_action(Action::AllIn).unwrap();
        state.apply_action(Action::AllIn).unwrap();
        state.showdown(&mut deck).unwrap();
        assert!(state.is_hand_over());

        let fragment = table_fragment(&state, 1, &[], &[]);
        assert!(
            fragment.contains(r#"class="pt-win"><b>WIN</b>"#),
            "winners carry a WIN badge at their seat: {fragment}"
        );
        assert!(
            fragment.matches("pt-winner").count() >= 1,
            "at least one seat is marked the winner: {fragment}"
        );
        for seat in Seat::ALL {
            let cards = state.hole_cards(seat).expect("all cards revealed");
            for card in cards {
                assert!(
                    fragment.contains(&format!(r#"data-code="{}""#, card)),
                    "{seat}'s cards are revealed at showdown"
                );
            }
        }
        assert!(
            !fragment.contains("pt-result"),
            "reveals render at the seats, not in a centre banner: {fragment}"
        );
    }
}
