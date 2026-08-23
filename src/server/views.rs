use crate::card::Card;
use crate::decision::AnalyzedDecision;
use crate::game::{Action, GameState, HandEndReason, Seat};
use crate::range::BetSize;

/// The full app shell page: dark table skin, placeholder top-bar chart, and
/// the containers the WS client swaps fragments into.
pub fn index_page() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer</title>
<script src="https://cdn.tailwindcss.com"></script>
<link rel="stylesheet" href="/assets/style.css">
</head>
<body class="bg-[#0b0e13] text-gray-200 min-h-screen">
  <header class="pt-topwrap">
    <div id="ws-status" class="status-wait">connecting…</div>
    <canvas id="ev-chart" width="1200" height="48" class="ev-chart"></canvas>
  </header>
  <main class="pt-main">
    <div id="table"></div>
    <div id="overlay"></div>
  </main>
  <script src="/assets/app.js"></script>
</body>
</html>"#
        .to_string()
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

fn card_html(card: Card) -> String {
    format!(
        r#"<span class="pt-card" data-suit="{:?}">{}</span>"#,
        card.suit(),
        escape(&card.to_code())
    )
}

/// The raw table-state HTML fragment swapped into the DOM on every state
/// change: top bar, seats, board, hero action panel, result, and action log.
pub fn table_fragment(state: &GameState, hand_no: u64, log: &[String]) -> String {
    let level = state.blind_level();
    let mut html = String::new();

    html.push_str(&format!(
        r#"<div id="table-state" class="table-shell" data-hand="{hand_no}">"#
    ));
    html.push_str(&format!(
        r#"<div class="pt-topbar"><span class="pt-pill">Hand #{hand_no}</span><span>Blinds {}/{} · {}</span><span class="pt-pot">Pot {}</span></div>"#,
        level.small_blind,
        level.big_blind,
        escape(&state.street().to_string()),
        state.total_pot()
    ));

    html.push_str(r#"<div class="pt-felt">"#);
    for seat in Seat::ALL {
        html.push_str(&seat_html(state, seat));
    }
    html.push_str(r#"<div class="pt-board">"#);
    for card in state.board() {
        html.push_str(&card_html(*card));
    }
    html.push_str("</div></div>");

    if state.is_hand_over() {
        html.push_str(&result_html(state));
    } else if state.to_act() == Seat::Hero {
        html.push_str(&action_panel(state));
    } else {
        html.push_str(&format!(
            r#"<div class="pt-wait">Waiting for {}…</div>"#,
            escape(&state.to_act().to_string())
        ));
    }

    html.push_str(r#"<div class="pt-log">"#);
    for line in log {
        html.push_str(&format!("<div>{}</div>", escape(line)));
    }
    html.push_str("</div>");

    html.push_str("</div>");
    html
}

fn seat_html(state: &GameState, seat: Seat) -> String {
    let active = !state.is_hand_over() && state.to_act() == seat;
    let button = if state.button() == seat {
        " · BTN"
    } else {
        ""
    };
    let cards = match seat {
        Seat::Hero => format!(
            "{} {}",
            card_html(state.hero_cards()[0]),
            card_html(state.hero_cards()[1])
        ),
        _ => match state.hole_cards(seat) {
            Some(cards) => format!("{} {}", card_html(cards[0]), card_html(cards[1])),
            None => r#"<span class="pt-card back">▮</span><span class="pt-card back">▮</span>"#
                .to_string(),
        },
    };
    let flags = if state.folded(seat) {
        "folded"
    } else if state.all_in(seat) {
        "all-in"
    } else {
        ""
    };
    format!(
        r#"<div class="pt-seat{active}" data-seat="{}"><div class="pt-seat-name">{}{}</div><div class="pt-seat-cards">{}</div><div class="pt-stack">{} chips <span class="pt-flags">{}</span></div></div>"#,
        escape(&seat.to_string()),
        escape(&seat.to_string()),
        button,
        cards,
        state.stack(seat),
        flags
    )
}

fn action_panel(state: &GameState) -> String {
    let legal = state.legal_actions();
    let mut html = String::from(r#"<div id="action-panel" class="action-panel">"#);

    if legal.can_fold {
        html.push_str(r#"<button class="action-btn danger" data-kind="fold">Fold</button>"#);
    }
    if legal.can_check {
        html.push_str(r#"<button class="action-btn" data-kind="check">Check</button>"#);
    }
    if legal.can_call {
        html.push_str(&format!(
            r#"<button class="action-btn" data-kind="call">Call {}</button>"#,
            legal.call_amount
        ));
    }

    let kind = if legal.can_bet {
        "bet"
    } else if legal.can_raise {
        "raise"
    } else {
        ""
    };
    if !kind.is_empty() {
        let to_call = if kind == "bet" { 0 } else { legal.call_amount };
        let min = if kind == "bet" {
            legal.min_bet
        } else {
            legal.min_raise_to
        };
        let max = if kind == "bet" {
            legal.max_bet
        } else {
            legal.max_raise_to
        };
        for bucket in [BetSize::Min, BetSize::HalfPot, BetSize::Pot, BetSize::AllIn] {
            let amount = bucket.to_raise_to(
                state.total_pot(),
                to_call,
                state.blind_level().big_blind,
                min,
                state.stack(Seat::Hero),
            );
            html.push_str(&format!(
                r#"<button class="action-btn" data-kind="{kind}" data-bucket="{}">{amount}</button>"#,
                escape(bucket.label()),
            ));
        }
        html.push_str(&format!(
            r#"<input id="custom-amount" class="amount-input" type="number" min="{min}" max="{max}" value="{min}"><button class="action-btn accent" data-kind="custom" data-custom-kind="{kind}">Bet amount</button>"#
        ));
    }

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

/// The tactical-breakdown fragment overlaid on the table: played vs optimal
/// action, the EV given up, and the survivability-ranked candidate table.
/// Intercepted blunders (S8) freezes the table: the modal is titled
/// accordingly and only offers a confirmation that unlocks the transition.
pub fn tactical_overlay_fragment(
    hand_no: u64,
    decision: &AnalyzedDecision,
    intercepted: bool,
) -> String {
    let optimal = decision.optimal;
    let mut html = String::from(r#"<div id="tactical-overlay" class="pt-overlay">"#);
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
        assert!(page.contains(r#"<div id="overlay"></div>"#));
        assert!(page.contains(r#"/assets/app.js"#));
    }

    #[test]
    fn table_fragment_reflects_the_current_state() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(31)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let fragment = table_fragment(&state, 3, &["You check".to_string()]);
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
        assert!(
            fragment.contains(&format!("{}", hero_cards[0])),
            "hero cards are visible"
        );
        assert!(
            fragment.contains(r#"class="pt-card back"#),
            "opponent cards stay hidden"
        );
    }

    #[test]
    fn table_fragment_shows_waiting_and_results_when_appropriate() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(32)))
            .unwrap();
        assert_ne!(state.to_act(), Seat::Hero);
        let waiting = table_fragment(&state, 1, &[]);
        assert!(waiting.contains("Waiting for"));

        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        let finished = table_fragment(&state, 1, &[]);
        assert!(finished.contains("win 30 — everyone else folded"));
        assert!(!finished.contains(r#"id="action-panel""#));
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
        let mut deck = Deck::shuffled(&mut seeded_rng(33));
        state.start_hand(&mut deck).unwrap();
        state.apply_action(Action::AllIn).unwrap();
        state.apply_action(Action::AllIn).unwrap();
        state.apply_action(Action::AllIn).unwrap();
        state.showdown(&mut deck).unwrap();
        assert!(state.is_hand_over());

        let fragment = table_fragment(&state, 1, &[]);
        assert!(
            fragment.contains("Showdown ·"),
            "winners are listed: {fragment}"
        );
        for card in state.hero_cards() {
            assert!(
                fragment.contains(&format!("{card}")),
                "hero cards are revealed at showdown"
            );
        }
    }
}
