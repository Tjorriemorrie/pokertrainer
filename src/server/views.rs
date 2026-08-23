use crate::analytics::{ChartPoint, SessionSummary};
use crate::card::{Card, Suit};
use crate::decision::AnalyzedDecision;
use crate::game::{Action, GameState, HandEndReason, Seat, Street};
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
<link rel="stylesheet" href="/assets/style.css?v=3">
</head>
<body class="pt-body">
  <header class="pt-topwrap">
    <div class="pt-brand">Poker Trainer</div>
    <canvas id="ev-chart" width="1200" height="48" class="ev-chart"></canvas>
    <div id="ws-status" class="status-wait">connecting…</div>
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
            appears here — the table stays fully visible next to it.
          </div>
        </div>
      </aside>
    </div>
  </main>
  <script src="/assets/app.js?v=2"></script>
</body>
</html>"#
        .to_string()
}

/// The finished-tournament history page (S9): one server-rendered card per
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
<link rel="stylesheet" href="/assets/style.css?v=3">
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
    <span class="pt-tournament-meta">{} hands · {} actions · avg EV loss {:.2}</span>
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

fn avatar_label(seat: Seat) -> String {
    match seat {
        Seat::Hero => "H".to_string(),
        Seat::Opponent1 => "O1".to_string(),
        Seat::Opponent2 => "O2".to_string(),
    }
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
/// and a collapsible action log. `sounds` carries the WebAudio cues the
/// client synthesizes for this update.
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

    if state.is_hand_over() {
        html.push_str(&result_html(state));
    } else if state.to_act() != Seat::Hero {
        html.push_str(&format!(
            r#"<div class="pt-wait">Waiting for {}…</div>"#,
            escape(&state.to_act().to_string())
        ));
    }

    html.push_str(
        r#"<button class="pt-log-toggle" type="button" data-log-toggle>History <span>▾</span></button>"#,
    );
    html.push_str(r#"<div class="pt-log">"#);
    for line in log {
        html.push_str(&format!("<div>{}</div>", escape(line)));
    }
    html.push_str("</div>");

    html.push_str("</div></div>");

    if !state.is_hand_over() && state.to_act() == Seat::Hero {
        html.push_str(r#"<div class="pt-action-block">"#);
        html.push_str(&action_panel(state));
        html.push_str("</div>");
    }

    html.push_str("</div>");
    html
}

fn seat_html(state: &GameState, seat: Seat) -> String {
    let active = !state.is_hand_over() && state.to_act() == seat;
    let level = state.blind_level();
    let folded = state.folded(seat);
    let all_in = state.all_in(seat);
    let stack = state.stack(seat);

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

    let cls = if active {
        "pt-seat pt-active"
    } else {
        "pt-seat"
    };
    let seat_name = escape(&seat.to_string());
    format!(
        r#"<div class="{cls}" data-seat="{seat_name}">
<div class="pt-avatar">{avatar}{flag}</div>
<div class="pt-seat-name">{seat_name}{badges}</div>
<div class="pt-seat-cards">{cards}</div>
<div class="pt-stack"><i class="pt-chip-dot"></i>{stack_text}</div>
{bet_html}
</div>"#,
        avatar = avatar_label(seat),
        seat_name = seat_name,
        flag = flag,
        badges = badges,
        cards = cards,
        stack_text = stack_text(stack, level.big_blind),
        bet_html = bet_html
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
                r#"<button type="button" class="pt-chip-size allin" data-size="{stack}">All-in</button>"#
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

fn result_html(state: &GameState) -> String {
    let Some(result) = state.hand_result() else {
        return String::new();
    };
    let total: u32 = result.awards.iter().map(|award| award.amount).sum();
    let mut html = r#"<div class="pt-result">"#.to_string();
    match result.reason {
        HandEndReason::Fold(winner) => {
            html.push_str(&format!("{winner} win {total} — everyone else folded"))
        }
        HandEndReason::Showdown => {
            let winners: Vec<String> = result
                .awards
                .iter()
                .map(|award| format!("{} +{}", award.seat, award.amount))
                .collect();
            html.push_str(&format!("Showdown · {}", winners.join(" · ")));
            for (seat, cards, _class) in &result.revealed {
                html.push_str(&format!(
                    r#"<div class="pt-reveal">{}: {} {}</div>"#,
                    seat,
                    card_html(cards[0]),
                    card_html(cards[1])
                ));
            }
        }
    }
    html.push_str("</div>");
    html
}

/// The tactical-breakdown fragment rendered into the coach-feedback panel
/// beside the table: played vs optimal action, the EV given up, and the
/// survivability-ranked candidate table. Intercepted blunders (S8) freeze the
/// table: the card is titled accordingly and only offers a confirmation that
/// unlocks the transition.
pub fn tactical_overlay_fragment(
    hand_no: u64,
    decision: &AnalyzedDecision,
    intercepted: bool,
) -> String {
    let optimal = decision.optimal;
    let mut html = String::from(r#"<div id="tactical-overlay" class="pt-feedback-card">"#);
    html.push_str(r#"<div class="pt-overlay-card">"#);
    if intercepted {
        html.push_str(&format!(
            r#"<h2 class="pt-overlay-title">Hand #{hand_no} — Blunder intercepted</h2>"#
        ));
        html.push_str(
            r#"<div class="pt-intercept-note">The table is paused. Review the blunder below before continuing.</div>"#,
        );
    } else {
        html.push_str(&format!(
            r#"<h2 class="pt-overlay-title">Hand #{hand_no} — Decision review</h2>"#
        ));
    }

    if let Some(played) = &decision.played {
        html.push_str(&format!(
            r#"<div class="pt-compare"><div class="pt-played">You played <b>{}</b> — EV {:.1}</div><div class="pt-optimal">Optimal: <b>{}</b> — EV {:.1}</div></div>"#,
            escape(&action_label(played.analysis.action)),
            played.analysis.ev,
            escape(&action_label(optimal.action)),
            optimal.ev
        ));
        html.push_str(&format!(
            r#"<div class="pt-ev-loss">EV lost: <b>{:.1}</b> chips</div>"#,
            played.ev_loss
        ));
    } else {
        html.push_str(&format!(
            r#"<div class="pt-compare"><div class="pt-optimal">Optimal: <b>{}</b> — EV {:.1}</div></div>"#,
            escape(&action_label(optimal.action)),
            optimal.ev
        ));
    }

    html.push_str(
        r#"<table class="pt-ranking"><tr><th>Action</th><th>EV</th><th>σ</th><th>Bust</th></tr>"#,
    );
    for analysis in &decision.ranking {
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
            r#"<tr class="{row_class}"><td>{}</td><td>{:.1}</td><td>{:.0}</td><td>{:.1}%</td></tr>"#,
            escape(&action_label(analysis.action)),
            analysis.ev,
            analysis.sigma(),
            analysis.bust_prob * 100.0
        ));
    }
    html.push_str("</table>");

    if intercepted {
        html.push_str(
            r#"<button class="action-btn pt-confirm" data-overlay-confirm>I understand — continue</button>"#,
        );
    } else {
        html.push_str(r#"<button class="action-btn" data-overlay-close>Continue</button>"#);
    }
    html.push_str("</div></div>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Deck;
    use crate::card::Rank;
    use crate::decision::{Analysis, PlayedEvaluation};
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
            "the S9 finish control is present"
        );
        assert!(
            page.contains(r#"href="/tournaments""#),
            "the tournament history link is present"
        );
        assert!(
            page.contains(r#"id="sound-toggle""#),
            "the S10 sound toggle is present"
        );
        assert!(
            page.contains(r#"/assets/style.css?v=3"#),
            "the stylesheet link is versioned so browsers drop stale cached CSS"
        );
        assert!(
            !page.contains("cdn.tailwindcss.com"),
            "the S10 skin ships its own CSS and works offline"
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
        assert!(page.contains("3 hands · 3 actions · avg EV loss 12.50"));
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
    fn table_fragment_shows_waiting_and_results_when_appropriate() {
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
        assert!(finished.contains("win 30 — everyone else folded"));
        assert!(!finished.contains(r#"id="action-panel""#));
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
        let felt_marker = fragment.find(r#"<button class="pt-log-toggle""#).unwrap();
        let dock = fragment.find(r#"id="action-panel""#).unwrap();
        assert!(
            dock > felt_marker,
            "the action panel renders in its own block below the oval: {fragment}"
        );
        assert!(
            fragment.contains(r#"<div class="pt-action-block"><div id="action-panel""#),
            "{fragment}"
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
                ev_loss: 18.0,
                is_optimal: false,
            }),
        }
    }

    #[test]
    fn tactical_overlay_compares_played_and_optimal() {
        let fragment = tactical_overlay_fragment(7, &sample_analysis(), false);
        assert!(fragment.contains("Hand #7 — Decision review"));
        assert!(fragment.contains("You played <b>Call</b>"));
        assert!(fragment.contains("Optimal: <b>Fold</b>"));
        assert!(fragment.contains("EV lost: <b>18.0</b> chips"));
        assert!(fragment.contains(r#"<tr class="optimal"><td>Fold</td>"#));
        assert!(fragment.contains(r#"<tr class="played"><td>Call</td>"#));
        assert!(fragment.contains(r#"data-overlay-close"#));
        assert!(
            fragment.contains(r#"class="pt-feedback-card""#),
            "the breakdown renders in the coach panel beside the table"
        );
    }

    #[test]
    fn intercepted_overlay_is_titled_flagged_and_only_confirms() {
        let fragment = tactical_overlay_fragment(7, &sample_analysis(), true);
        assert!(fragment.contains("Hand #7 — Blunder intercepted"));
        assert!(fragment.contains("The table is paused"));
        assert!(fragment.contains(r#"data-overlay-confirm"#));
        assert!(fragment.contains("I understand — continue"));
        assert!(
            !fragment.contains(r#"data-overlay-close"#),
            "an intercepted modal cannot be silently dismissed"
        );
    }

    #[test]
    fn tactical_overlay_handles_a_missing_played_action() {
        let mut decision = sample_analysis();
        decision.played = None;
        let fragment = tactical_overlay_fragment(7, &decision, false);
        assert!(!fragment.contains("EV lost"));
        assert!(fragment.contains("Optimal: <b>Fold</b>"));
    }

    #[test]
    fn showdown_fragment_reveals_cards_and_awards() {
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
            fragment.contains("Showdown ·"),
            "winners are listed: {fragment}"
        );
        for card in state.hero_cards() {
            assert!(
                fragment.contains(&format!(r#"data-code="{}""#, card)),
                "hero cards are revealed at showdown"
            );
        }
    }
}
