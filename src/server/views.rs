use crate::analytics::{ChartPoint, SessionSummary, TournamentDetail};
use crate::card::{Card, Suit};
use crate::decision::{Analysis, AnalyzedDecision, SearchReport};
use crate::error::Result;
use crate::game::{Action, GameState, Seat, Street};
use crate::opponent::OpponentSnapshot;
use crate::range::BetSize;
use crate::server::session::Sound;
use crate::snapshot::ActiveSummary;
use askama::Template;

/// The dashboard landing page: either the resume card for the one active
/// tournament or a fresh **Start tournament** button. A new tournament can
/// only start once the previous one is finished (won, lost, or given up via
/// **Finish table**).
#[derive(Template)]
#[template(path = "pages/dashboard.html")]
struct DashboardTemplate<'a> {
    active: Option<&'a ActiveSummary>,
}

pub fn dashboard_page(active: Option<&ActiveSummary>) -> Result<String> {
    Ok(DashboardTemplate { active }.render()?)
}

/// The opponent-analysis page shell: a polling container filled by
/// [`analysis_status_html`] fragments fetched from the status endpoint.
#[derive(Template)]
#[template(path = "pages/analysis.html")]
struct AnalysisTemplate;

pub fn analysis_page() -> Result<String> {
    Ok(AnalysisTemplate.render()?)
}

/// The full table shell page: GGPoker-dark skin, top-bar lifetime EV chart (with
/// the hero-vs-field skill chip beside it), the table controls (finish,
/// tournament history, sound toggle), the table column docked top-left, and the
/// coach-feedback panel beside it (never covering the table).
#[derive(Template)]
#[template(path = "pages/play.html")]
struct PlayTemplate {
    /// `None` when the app has no analytics store to derive either number from,
    /// which suppresses the header chip entirely.
    skill: Option<SkillChip>,
}

/// The top-bar chip comparing the hero's lifetime skill against the bot
/// template's field skill, on the same 0..1 scale.
struct SkillChip {
    you: String,
    bots: String,
}

pub fn play_page(you: Option<f64>, bots: Option<f64>) -> Result<String> {
    Ok(PlayTemplate {
        skill: match (you, bots) {
            (None, None) => None,
            (you, bots) => Some(SkillChip {
                you: format_skill(you),
                bots: format_skill(bots),
            }),
        },
    }
    .render()?)
}

/// Formats one skill value for the header chip: two decimals, or an em dash
/// when the value does not exist yet.
fn format_skill(skill: Option<f64>) -> String {
    skill
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "—".to_string())
}

/// One card of the tournaments listing. Floats and the chart dataset are
/// pre-formatted here so `std::fmt` (not the template) decides the rounding.
struct TournamentCard {
    id: i32,
    /// Suffix appended to `pt-result-badge`, e.g. `" win"`. Empty with
    /// `badge_label` means the session finished without a decided outcome and
    /// the listing shows no badge at all.
    badge_class: &'static str,
    badge_label: &'static str,
    started: String,
    ended: String,
    hands: i32,
    actions: i64,
    avg_ev_loss: String,
    dataset: String,
}

impl TournamentCard {
    fn new(summary: &SessionSummary, points: &[ChartPoint]) -> Self {
        let (badge_class, badge_label) = match summary.result.as_deref() {
            Some("WIN") => (" win", "WIN"),
            Some("LOSS") => (" loss", "LOSS"),
            _ => ("", ""),
        };
        Self {
            id: summary.id,
            badge_class,
            badge_label,
            started: summary.started.clone(),
            ended: summary.ended.clone(),
            hands: summary.hands,
            actions: summary.actions,
            avg_ev_loss: format!("{:.2}", summary.avg_ev_loss),
            dataset: serde_json::to_string(points).unwrap_or_else(|_| "[]".to_string()),
        }
    }
}

/// The finished-tournament history page: a paginated listing (newest first)
/// of one server-rendered card per finished session whose decimated EV
/// dataset is drawn client-side with the same canvas style as the live
/// top-bar chart. `page`/`pages` drive the Newer/Older navigation.
#[derive(Template)]
#[template(path = "pages/tournaments.html")]
struct TournamentsTemplate {
    cards: Vec<TournamentCard>,
    page: u32,
    pages: u32,
}

pub fn tournaments_page(
    sessions: &[(SessionSummary, Vec<ChartPoint>)],
    page: u32,
    pages: u32,
) -> Result<String> {
    Ok(TournamentsTemplate {
        cards: sessions
            .iter()
            .map(|(summary, points)| TournamentCard::new(summary, points))
            .collect(),
        page,
        pages,
    }
    .render()?)
}

/// The single-tournament detail page: the outcome, hand-level aggregates
/// (hands, wins, losses, all-in frequency), EV stats, and the decimated
/// action-EV chart.
#[derive(Template)]
#[template(path = "pages/tournament_detail.html")]
struct TournamentDetailTemplate {
    id: i32,
    /// Unlike the listing, this page always shows a badge — an em dash stands
    /// in for a session that ended without a decided outcome.
    badge_class: &'static str,
    badge_label: &'static str,
    started: String,
    ended: String,
    final_stack: String,
    hands: i64,
    hands_won: i64,
    hands_lost: i64,
    win_rate: String,
    all_ins: i64,
    all_in_pct: String,
    avg_ev_loss: String,
    total_ev_loss: String,
    max_ev_loss: String,
    dataset: String,
}

pub fn tournament_detail_page(detail: &TournamentDetail) -> Result<String> {
    let summary = &detail.summary;
    let win_rate = if detail.hands > 0 {
        detail.hands_won as f64 * 100.0 / detail.hands as f64
    } else {
        0.0
    };
    let (badge_class, badge_label) = match summary.result.as_deref() {
        Some("WIN") => (" win", "WIN"),
        Some("LOSS") => (" loss", "LOSS"),
        _ => ("", "—"),
    };
    Ok(TournamentDetailTemplate {
        id: summary.id,
        badge_class,
        badge_label,
        started: summary.started.clone(),
        ended: summary.ended.clone(),
        final_stack: summary
            .final_stack
            .map(|stack| format!("{stack} chips"))
            .unwrap_or_else(|| "—".to_string()),
        hands: detail.hands,
        hands_won: detail.hands_won,
        hands_lost: detail.hands_lost,
        win_rate: format!("{win_rate:.0}"),
        all_ins: detail.all_ins,
        all_in_pct: format!("{:.0}", detail.all_in_pct),
        avg_ev_loss: format!("{:.2}", summary.avg_ev_loss),
        total_ev_loss: format!("{:.2}", detail.total_ev_loss),
        max_ev_loss: format!("{:.2}", detail.max_ev_loss),
        dataset: serde_json::to_string(&detail.points).unwrap_or_else(|_| "[]".to_string()),
    }
    .render()?)
}

/// One `pt-stat-card`. Values arrive pre-formatted so `std::fmt` (not the
/// template) decides the rounding.
struct Stat {
    label: &'static str,
    value: String,
}

impl Stat {
    fn new(label: &'static str, value: impl std::fmt::Display) -> Self {
        Self {
            label,
            value: value.to_string(),
        }
    }
}

/// The chip describing the stored bot template, shown only when one exists.
struct TemplateChip {
    label: String,
    skill: String,
    avg_ev_loss_bb: String,
    decisions: i32,
}

/// One row of the imported-tournament listing.
struct TournamentRow {
    date: String,
    id: String,
    name: String,
    game_type: Option<String>,
    entrants: String,
    buy_in: String,
    place: String,
    prize: String,
    profit: String,
    profit_class: &'static str,
    hands: i64,
    hands_won: i64,
    win_pct: String,
    net_class: &'static str,
    net_chips: String,
}

impl TournamentRow {
    fn new(row: &crate::hh::TournamentListing) -> Self {
        let tournament = &row.tournament;
        let profit = match (tournament.buy_in_cents, tournament.prize_cents) {
            (Some(buy), Some(prize)) => crate::hh::money(i64::from(prize) - i64::from(buy)),
            _ => "—".to_string(),
        };
        Self {
            date: tournament
                .finished
                .clone()
                .unwrap_or_else(|| tournament.started.clone()),
            id: tournament.id.clone(),
            name: tournament.name.clone(),
            game_type: tournament.game_type.clone(),
            entrants: tournament
                .entrants
                .map_or_else(String::new, |n| format!("{n} players")),
            buy_in: cents_or_dash(tournament.buy_in_cents),
            place: tournament
                .place
                .map(ordinal)
                .unwrap_or_else(|| "—".to_string()),
            prize: cents_or_dash(tournament.prize_cents),
            profit_class: if profit.starts_with('-') {
                "pt-neg"
            } else if profit.starts_with('$') {
                "pt-pos"
            } else {
                ""
            },
            profit,
            hands: row.hands,
            hands_won: row.hands_won,
            win_pct: format!("{:.0}", pct(row.hands_won, row.hands)),
            net_class: if row.net_chips < 0 {
                "pt-neg"
            } else {
                "pt-pos"
            },
            net_chips: signed(row.net_chips),
        }
    }
}

/// Formats an optional cent amount as money, or an em dash when absent.
fn cents_or_dash(cents: Option<i32>) -> String {
    cents
        .map(|cents| crate::hh::money(i64::from(cents)))
        .unwrap_or_else(|| "—".to_string())
}

/// The GGPoker hand-history page: the scan trigger, the opponent-skill
/// analyzer entry and the current bot template, the lifetime
/// profit/win-rate aggregates, and one row per imported tournament (newest
/// first) linking to its hand-level detail page.
#[derive(Template)]
#[template(path = "pages/history.html")]
struct HistoryTemplate {
    cards: Vec<Stat>,
    chip: Option<TemplateChip>,
    rows: Vec<TournamentRow>,
}

pub fn history_page(
    stats: &crate::hh::OverallStats,
    tournaments: &[crate::hh::TournamentListing],
    template: Option<&crate::opponent_analysis::DrillTemplate>,
) -> Result<String> {
    let profit = stats.prize_cents - stats.buy_in_cents;
    Ok(HistoryTemplate {
        cards: vec![
            Stat::new("Tournaments", stats.tournaments),
            Stat::new(
                "Tournaments won",
                format!(
                    "{} ({}%)",
                    stats.tournaments_won,
                    round1(pct(stats.tournaments_won, stats.tournaments))
                ),
            ),
            Stat::new("Hands", stats.hands),
            Stat::new(
                "Hands won",
                format!(
                    "{} ({}%)",
                    stats.hands_won,
                    round1(pct(stats.hands_won, stats.hands))
                ),
            ),
            Stat::new("Net profit", crate::hh::money(profit)),
            Stat::new("Buy-ins", crate::hh::money(stats.buy_in_cents)),
            Stat::new("Prizes", crate::hh::money(stats.prize_cents)),
            Stat::new("All-ins", stats.all_ins),
            Stat::new("Showdowns", stats.showdowns),
            Stat::new("Net chips", signed(stats.net_chips)),
        ],
        chip: template.map(|template| TemplateChip {
            label: template.label.clone(),
            skill: format!("{:.2}", template.skill),
            avg_ev_loss_bb: format!("{:.2}", template.avg_ev_loss_bb),
            decisions: template.decisions,
        }),
        rows: tournaments.iter().map(TournamentRow::new).collect(),
    }
    .render()?)
}

/// The scan-results page: what the scan found and stored, plus statistics
/// over the newly imported hands only (already-imported hands add nothing).
#[derive(Template)]
#[template(path = "pages/history_scan_result.html")]
struct HistoryScanResultTemplate {
    summary_cards: Vec<Stat>,
    hand_cards: Vec<Stat>,
    failures: Vec<String>,
}

pub fn history_scan_result_page(outcome: &crate::hh::ImportOutcome) -> Result<String> {
    let stats = &outcome.new_stats;
    Ok(HistoryScanResultTemplate {
        summary_cards: vec![
            Stat::new("ZIPs scanned", outcome.zips),
            Stat::new("Files read", outcome.files),
            Stat::new("Hands parsed", outcome.hands_parsed),
            Stat::new("New hands", outcome.hands_new),
            Stat::new("Already imported", outcome.hands_skipped),
            Stat::new("New tournaments", outcome.tournaments_new),
        ],
        hand_cards: vec![
            Stat::new("Hands", stats.hands),
            Stat::new("Won", stats.won),
            Stat::new("Lost", stats.lost),
            Stat::new("Win ratio", format!("{}%", round1(stats.win_ratio))),
            Stat::new("All-ins", stats.all_ins),
            Stat::new("Showdowns", stats.showdowns),
            Stat::new("Chips invested", stats.invested),
            Stat::new("Chips collected", stats.collected),
            Stat::new("Chips won/lost", signed(stats.net_chips)),
            Stat::new("Tournaments", stats.tournaments),
        ],
        failures: outcome.failures.clone(),
    }
    .render()?)
}

/// The status fragment swapped into the analysis page: idle nudge, live
/// progress, or the finished report with the save-template action.
#[derive(Template)]
#[template(path = "fragments/analysis_idle.html")]
struct AnalysisIdleFragment;

#[derive(Template)]
#[template(path = "fragments/analysis_running.html")]
struct AnalysisRunningFragment {
    hands_done: u32,
    hands_total: u32,
    pct: String,
}

/// One opponent's row in the finished report.
struct PlayerRowView {
    name: String,
    decisions: u32,
    avg_ev_loss_bb: String,
}

#[derive(Template)]
#[template(path = "fragments/analysis_done.html")]
struct AnalysisDoneFragment {
    cards: Vec<Stat>,
    players: Vec<PlayerRowView>,
    problems: Vec<String>,
    /// `Some` only when the run graded at least one decision, which is what
    /// gates the save-template action.
    save_skill: Option<String>,
}

/// The status fragment swapped into the analysis page: idle nudge, live
/// progress, or the finished report with the save-template action. Each shape
/// is its own template, so the variants stay independently checked.
pub fn analysis_status_html(status: &crate::opponent_analysis::JobState) -> Result<String> {
    use crate::opponent_analysis::JobState;

    Ok(match status {
        JobState::Idle => AnalysisIdleFragment.render()?,
        JobState::Running {
            hands_done,
            hands_total,
        } => {
            let pct = if *hands_total == 0 {
                0.0
            } else {
                (*hands_done as f64 * 100.0) / (*hands_total as f64)
            };
            AnalysisRunningFragment {
                hands_done: *hands_done,
                hands_total: *hands_total,
                pct: format!("{pct:.0}"),
            }
            .render()?
        }
        JobState::Done(report) => AnalysisDoneFragment {
            cards: vec![
                Stat::new("Hands in window", report.hands_total),
                Stat::new("Hands graded", report.hands_graded),
                Stat::new("Hands skipped", report.hands_failed),
                Stat::new("Opponent decisions", report.decisions),
                Stat::new(
                    "Avg BB lost per decision",
                    format!("{:.3}", report.avg_ev_loss_bb),
                ),
                Stat::new("Field skill", format!("{:.2}", report.skill)),
            ],
            players: report
                .players
                .iter()
                .map(|player| PlayerRowView {
                    name: player.name.clone(),
                    decisions: player.decisions,
                    avg_ev_loss_bb: format!("{:.3}", player.avg_ev_loss_bb),
                })
                .collect(),
            problems: report.problems.clone(),
            save_skill: (report.decisions > 0).then(|| format!("{:.2}", report.skill)),
        }
        .render()?,
    })
}

/// One imported tournament's detail page: the stored summary, its aggregate
/// stats, and every hand newest first.
/// One row of a tournament's hand table, pre-formatted.
struct HandRowView {
    played_at: String,
    sb: i32,
    bb: i32,
    position: String,
    table_size: i32,
    cards: String,
    all_in: String,
    showdown: String,
    invested: i32,
    collected: i32,
    net_class: &'static str,
    net: String,
    board: String,
}

#[derive(Template)]
#[template(path = "pages/history_tournament_detail.html")]
struct HistoryTournamentDetailTemplate {
    id: String,
    name: String,
    /// This page derives the badge from the finishing place: 1st is a WIN, any
    /// other place a LOSS, and an unknown place an em dash.
    badge_class: &'static str,
    badge_label: &'static str,
    started: String,
    finished: String,
    buy_in: String,
    prize: String,
    profit: String,
    cards: Vec<Stat>,
    hands: Vec<HandRowView>,
}

pub fn history_tournament_detail_page(detail: &crate::hh::TournamentDetail) -> Result<String> {
    let listing = &detail.listing;
    let tournament = &listing.tournament;
    let (badge_class, badge_label) = match tournament.place {
        Some(1) => (" win", "WIN"),
        Some(_) => (" loss", "LOSS"),
        None => ("", "—"),
    };
    Ok(HistoryTournamentDetailTemplate {
        id: tournament.id.clone(),
        name: tournament.name.clone(),
        badge_class,
        badge_label,
        started: tournament.started.clone(),
        finished: tournament
            .finished
            .clone()
            .unwrap_or_else(|| "?".to_string()),
        buy_in: cents_or_dash(tournament.buy_in_cents),
        prize: cents_or_dash(tournament.prize_cents),
        profit: match (tournament.buy_in_cents, tournament.prize_cents) {
            (Some(buy), Some(prize)) => crate::hh::money(i64::from(prize) - i64::from(buy)),
            _ => "—".to_string(),
        },
        cards: vec![
            Stat::new("Hands", listing.hands),
            Stat::new("Hands won", listing.hands_won),
            Stat::new(
                "Win rate",
                format!("{:.0}%", pct(listing.hands_won, listing.hands)),
            ),
            Stat::new("All-ins", listing.all_ins),
            Stat::new("Showdowns", listing.showdowns),
            Stat::new("Net chips", signed(listing.net_chips)),
        ],
        hands: detail
            .hands
            .iter()
            .map(|hand| HandRowView {
                played_at: hand.played_at.clone(),
                sb: hand.sb,
                bb: hand.bb,
                position: hand.position.clone(),
                table_size: hand.table_size,
                cards: hand.hero_cards.clone().unwrap_or_else(|| "—".to_string()),
                all_in: yes_no(hand.all_in),
                showdown: yes_no(hand.showdown),
                invested: hand.invested,
                collected: hand.collected,
                net_class: if hand.net < 0 { "pt-neg" } else { "pt-pos" },
                net: signed(i64::from(hand.net)),
                board: hand.board.clone().unwrap_or_default(),
            })
            .collect(),
    }
    .render()?)
}

fn yes_no(value: bool) -> String {
    if value { "yes" } else { "—" }.to_string()
}

/// `1` → `1st`, `2` → `2nd`, …
fn ordinal(place: i32) -> String {
    let suffix = match place % 100 {
        11..=13 => "th",
        _ => match place % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{place}{suffix}")
}

/// A signed chip count: `+300`, `-150`, `0`.
fn signed(chips: i64) -> String {
    if chips > 0 {
        format!("+{chips}")
    } else {
        chips.to_string()
    }
}

/// Percentage with a zero-division guard, 0..100.
fn pct(won: i64, total: i64) -> f64 {
    if total > 0 {
        won as f64 * 100.0 / total as f64
    } else {
        0.0
    }
}

/// Rounds a percentage to one decimal, `66.7`.
fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
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

fn sounds_json(sounds: &[Sound]) -> String {
    let tags: Vec<&str> = sounds.iter().map(|sound| sound.tag()).collect();
    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

/// The raw table-state HTML fragment swapped into the DOM on every state
/// change: a GGPoker-style oval felt with fixed seat positions (folded and
/// busted players stay seated), the board, the pot, the action dock in its
/// own right-aligned block below the oval (never covering the hero's cards)
/// with the live solver-depth badge docked top-left of the same block, and
/// an always-visible action log docked to the left of the oval, exactly
/// as tall as the table. `sounds` carries the WebAudio cues the client
/// synthesizes for this update.
/// One playing card, ready for the `card` macro. `suit_debug` is the `Debug`
/// spelling of the suit, which is what `assets/app.js` reads off `data-suit`.
struct CardView {
    suit_class: &'static str,
    suit_debug: String,
    code: String,
    rank: String,
    symbol: char,
}

impl CardView {
    fn new(card: Card) -> Self {
        let suit = card.suit();
        let code = card.to_code();
        Self {
            suit_class: match suit {
                Suit::Hearts => " red",
                Suit::Diamonds => " blue",
                Suit::Clubs => " green",
                Suit::Spades => "",
            },
            suit_debug: format!("{suit:?}"),
            rank: code[..1].to_string(),
            code,
            symbol: suit_symbol(suit),
        }
    }
}

/// One seat on the felt.
#[derive(Template)]
#[template(path = "fragments/seat.html")]
struct SeatFragment {
    cls: &'static str,
    name: String,
    is_button: bool,
    is_small_blind: bool,
    is_big_blind: bool,
    /// The shown hole cards, empty when the seat is eliminated or face-down.
    cards: Vec<CardView>,
    /// Face-down placeholders instead of `cards`.
    backs: bool,
    /// Empty `flag_label` means no flag at all.
    flag_class: &'static str,
    flag_label: &'static str,
    stack: u32,
    stack_bb: String,
    /// Zero suppresses the bet chip.
    bet: u32,
    win: Option<u32>,
}

impl SeatFragment {
    fn new(state: &GameState, seat: Seat) -> Self {
        let level = state.blind_level();
        let active = !state.is_hand_over() && state.to_act() == seat;
        let eliminated = state.eliminated(seat);

        let (cards, backs) = match seat {
            Seat::Hero => (
                state
                    .hero_cards()
                    .iter()
                    .copied()
                    .map(CardView::new)
                    .collect(),
                false,
            ),
            _ => match state.hole_cards(seat) {
                Some(cards) => (cards.iter().copied().map(CardView::new).collect(), false),
                None if eliminated => (Vec::new(), false),
                None => (Vec::new(), true),
            },
        };

        let (flag_class, flag_label) = if state.folded(seat) {
            ("", "Fold")
        } else if state.all_in(seat) {
            (" allin", "All-in")
        } else if eliminated {
            (" bust", "OUT")
        } else {
            ("", "")
        };

        let win = state.hand_result().and_then(|result| {
            result
                .awards
                .iter()
                .find(|award| award.seat == seat)
                .map(|award| award.amount)
        });

        let stack = state.stack(seat);
        Self {
            cls: match (active, win.is_some()) {
                (true, _) => "pt-seat pt-active",
                (false, true) => "pt-seat pt-winner",
                (false, false) => "pt-seat",
            },
            name: seat.to_string(),
            is_button: state.button() == seat,
            is_small_blind: seat == state.small_blind_seat(),
            is_big_blind: seat == state.big_blind_seat(),
            cards,
            backs,
            flag_class,
            flag_label,
            stack,
            stack_bb: format!("{:.1}", stack as f32 / level.big_blind as f32),
            bet: state.street_contribution(seat),
            win,
        }
    }
}

/// One pot-fraction or fixed sizing chip in the action dock.
struct SizeChip {
    bucket: &'static str,
    amount: u32,
    label: String,
}

/// The bet-sizing half of the action dock, present only when betting or raising
/// is legal. Which chips survive (clamping and dedup) and the slider bounds are
/// decided in Rust — that is real logic, not presentation.
struct SizingView {
    chips: Vec<SizeChip>,
    can_all_in: bool,
    min: u32,
    max: u32,
    initial: u32,
    kind: &'static str,
    is_bet: bool,
}

#[derive(Template)]
#[template(path = "fragments/action_panel.html")]
struct ActionPanelFragment {
    sizing: Option<SizingView>,
    can_fold: bool,
    can_check: bool,
    can_call: bool,
    call_amount: u32,
    /// The hero's stack, when an all-in button belongs in the action row rather
    /// than among the sizing chips.
    bare_all_in: Option<u32>,
}

impl ActionPanelFragment {
    fn new(state: &GameState) -> Self {
        let legal = state.legal_actions();
        let level = state.blind_level();
        let betting = legal.can_bet;
        let raising = legal.can_raise;
        let sizing = betting || raising;

        let sizing = sizing.then(|| {
            let to_call = if betting { 0 } else { legal.call_amount };
            let (min, max) = if betting {
                (legal.min_bet, legal.max_bet)
            } else {
                (legal.min_raise_to, legal.max_raise_to)
            };
            let stack = state.stack(Seat::Hero);
            let preflop = state.street() == Street::Preflop;
            let facing_raise =
                raising && to_call > 0 && (!preflop || state.current_bet() > level.big_blind);
            let pot_fractions = facing_raise || !preflop;
            let buckets: &[BetSize] = if pot_fractions {
                &[
                    BetSize::ThirdPot,
                    BetSize::HalfPot,
                    BetSize::ThreeQuarterPot,
                    BetSize::Pot,
                ]
            } else {
                &[
                    BetSize::Min,
                    BetSize::ThreeBb,
                    BetSize::FourBb,
                    BetSize::Pot,
                ]
            };
            let mut chips: Vec<SizeChip> = Vec::new();
            for bucket in buckets {
                let amount = bucket
                    .to_raise_to(state.total_pot(), to_call, level.big_blind, min, stack)
                    .clamp(min, max);
                if amount >= stack || chips.iter().any(|chip| chip.amount == amount) {
                    continue;
                }
                chips.push(SizeChip {
                    bucket: bucket.label(),
                    amount,
                    label: if pot_fractions {
                        pot_percent_label(*bucket).to_string()
                    } else {
                        amount.to_string()
                    },
                });
            }
            let default_bucket = if facing_raise && preflop {
                BetSize::TwoX
            } else if preflop {
                BetSize::ThreeBb
            } else {
                BetSize::HalfPot
            };
            SizingView {
                chips,
                can_all_in: legal.can_all_in,
                min,
                max,
                initial: default_bucket
                    .to_raise_to(state.total_pot(), to_call, level.big_blind, min, stack)
                    .clamp(min, max),
                kind: if betting { "bet" } else { "raise" },
                is_bet: betting,
            }
        });

        Self {
            can_fold: legal.can_fold,
            can_check: legal.can_check,
            can_call: legal.can_call,
            call_amount: legal.call_amount,
            bare_all_in: (sizing.is_none() && legal.can_all_in).then(|| state.stack(Seat::Hero)),
            sizing,
        }
    }
}

/// One line of the action log. Hand markers (`— Hand #N …`) get gold emphasis so
/// deals stand out between actions.
struct LogLine {
    class: &'static str,
    text: String,
}

/// The hero's turn: the decision token the client echoes back, plus the dock.
struct ActionBlockView {
    decision: String,
    panel: ActionPanelFragment,
}

#[derive(Template)]
#[template(path = "fragments/table.html")]
struct TableFragment {
    hand_no: u64,
    sounds_json: String,
    small_blind: u32,
    big_blind: u32,
    street: String,
    log: Vec<LogLine>,
    seats: Vec<SeatFragment>,
    /// Zero suppresses the pot chip.
    pot: u32,
    board: Vec<CardView>,
    waiting_for: Option<String>,
    action_block: Option<ActionBlockView>,
}

pub fn table_fragment(
    state: &GameState,
    hand_no: u64,
    action_no: u64,
    log: &[String],
    sounds: &[Sound],
) -> Result<String> {
    let level = state.blind_level();
    let hero_turn = !state.is_hand_over() && state.to_act() == Seat::Hero;
    Ok(TableFragment {
        hand_no,
        sounds_json: sounds_json(sounds),
        small_blind: level.small_blind,
        big_blind: level.big_blind,
        street: state.street().to_string(),
        log: log
            .iter()
            .map(|line| LogLine {
                class: if line.starts_with('—') {
                    "pt-hlog-line marker"
                } else {
                    "pt-hlog-line"
                },
                text: line.clone(),
            })
            .collect(),
        seats: Seat::ALL
            .iter()
            .map(|seat| SeatFragment::new(state, *seat))
            .collect(),
        pot: state.total_pot(),
        board: state.board().iter().copied().map(CardView::new).collect(),
        waiting_for: (!state.is_hand_over() && state.to_act() != Seat::Hero)
            .then(|| state.to_act().to_string()),
        action_block: hero_turn.then(|| ActionBlockView {
            decision: format!(
                "h{hand_no}-a{action_no}-{}",
                state.street().to_string().to_lowercase()
            ),
            panel: ActionPanelFragment::new(state),
        }),
    }
    .render()?)
}

/// The GGPoker-style percentage label for a pot-fraction sizing chip.
fn pot_percent_label(bucket: BetSize) -> &'static str {
    match bucket {
        BetSize::ThirdPot => "33%",
        BetSize::HalfPot => "50%",
        BetSize::ThreeQuarterPot => "75%",
        BetSize::Pot => "100%",
        _ => "",
    }
}

/// The tactical-breakdown fragment rendered into the coach-feedback panel
/// beside the table: the opponents' live HUD cards first, then a plain-English
/// takeaway sentence, the played vs optimal action comparison, and the
/// candidate table sorted from cheapest (fold first) to all-in. Intercepted
/// blunders freeze the table: the card is titled accordingly and only offers
/// a confirmation that unlocks the transition (the coach's best-EV action).
/// One opponent's HUD card.
struct OpponentView {
    name: String,
    is_button: bool,
    is_small_blind: bool,
    is_big_blind: bool,
    /// Empty `flag_label` means no status flag at all.
    flag_class: &'static str,
    flag_label: &'static str,
    stack: u32,
    stack_bb: String,
    hands: usize,
    vpip: String,
    pfr: String,
    fold_to_bet: String,
    aggression: String,
    read: String,
}

#[derive(Template)]
#[template(path = "fragments/opponents.html")]
struct OpponentsFragment {
    opponents: Vec<OpponentView>,
}

impl OpponentsFragment {
    fn new(opponents: &[OpponentSnapshot], big_blind: u32) -> Self {
        Self {
            opponents: opponents
                .iter()
                .map(|opponent| {
                    let (flag_class, flag_label) = if opponent.folded {
                        ("fold", "Folded")
                    } else if opponent.all_in {
                        ("allin", "All-in")
                    } else if opponent.stack == 0 {
                        ("bust", "OUT")
                    } else {
                        ("", "")
                    };
                    OpponentView {
                        name: opponent.seat.to_string(),
                        is_button: opponent.is_button,
                        is_small_blind: opponent.is_small_blind,
                        is_big_blind: opponent.is_big_blind,
                        flag_class,
                        flag_label,
                        stack: opponent.stack,
                        stack_bb: format!("{:.1}", opponent.stack as f32 / big_blind as f32),
                        hands: opponent.hands,
                        vpip: format!("{:.0}", opponent.vpip_pct),
                        pfr: format!("{:.0}", opponent.pfr_pct),
                        fold_to_bet: format!("{:.0}", opponent.fold_to_bet_pct),
                        aggression: match (opponent.postflop_bets, opponent.postflop_calls) {
                            (0, 0) => "—".to_string(),
                            (_, 0) => "∞".to_string(),
                            (bets, calls) => format!("{:.1}", bets as f64 / calls as f64),
                        },
                        read: opponent.read.clone(),
                    }
                })
                .collect(),
        }
    }
}

/// The hero's played action, when there was one to compare against.
struct PlayedView {
    label: String,
    ev: String,
    ev_loss_bb: String,
}

/// One row of the candidate table.
struct RankingRow {
    /// `optimal`, `played`, or empty.
    class: &'static str,
    label: String,
    ev: String,
    sigma: String,
    bust: String,
    visits: u64,
}

#[derive(Template)]
#[template(path = "fragments/tactical_overlay.html")]
struct TacticalOverlayFragment {
    hand_no: u64,
    intercepted: bool,
    opponents: OpponentsFragment,
    sentence: String,
    played: Option<PlayedView>,
    optimal_label: String,
    optimal_ev: String,
    ranking: Vec<RankingRow>,
    effort: SearchEffort,
}

pub fn tactical_overlay_fragment(
    hand_no: u64,
    decision: &AnalyzedDecision,
    intercepted: bool,
    opponents: &[OpponentSnapshot],
    big_blind: u32,
    call_amount: u32,
    hero_stack: u32,
) -> Result<String> {
    let optimal = decision.optimal;
    let mut rows: Vec<&Analysis> = decision.ranking.iter().collect();
    rows.sort_by_key(|analysis| chip_cost(analysis.action, call_amount, hero_stack));
    Ok(TacticalOverlayFragment {
        hand_no,
        intercepted,
        opponents: OpponentsFragment::new(opponents, big_blind),
        sentence: ev_diff_sentence(decision),
        played: decision.played.as_ref().map(|played| PlayedView {
            label: action_label(played.analysis.action),
            ev: format!("{:.1}", played.analysis.ev),
            ev_loss_bb: format!("{:.2}", played.ev_loss_bb),
        }),
        optimal_label: action_label(optimal.action),
        optimal_ev: format!("{:.1}", optimal.ev),
        ranking: rows
            .into_iter()
            .map(|analysis| RankingRow {
                class: if analysis.action == optimal.action {
                    "optimal"
                } else if decision
                    .played
                    .as_ref()
                    .is_some_and(|played| played.analysis.action == analysis.action)
                {
                    "played"
                } else {
                    ""
                },
                label: action_label(analysis.action),
                ev: format!("{:.1}", analysis.ev),
                sigma: format!("{:.0}", analysis.sigma()),
                bust: format!("{:.1}", analysis.bust_prob * 100.0),
                visits: analysis.visits,
            })
            .collect(),
        effort: search_effort(&decision.search),
    }
    .render()?)
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
    match decision.played.as_ref() {
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
    }
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
    fn play_page_shell_points_at_the_ws_client() {
        let page = play_page(None, None).unwrap();
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
            !page.contains(r#"id="mcts-status""#),
            "the solver depth badge lives in the action dock, not the header shell"
        );
        assert!(
            page.contains(r#"/assets/style.css?v=13"#),
            "the stylesheet link is versioned so browsers drop stale cached CSS"
        );
        assert!(
            !page.contains("cdn.tailwindcss.com"),
            "the skin ships its own CSS and works offline"
        );
    }

    fn active_summary(with_tournament: bool) -> Option<ActiveSummary> {
        with_tournament.then(|| ActiveSummary {
            session_id: 9,
            hand_no: 12,
            street: Street::Flop,
            blind_small: 10,
            blind_big: 20,
            hero_stack: 460,
            active_opponents: 2,
            actions: 41,
            started: "2026-08-24T10:00:00Z".to_string(),
        })
    }

    #[test]
    fn dashboard_without_an_active_tournament_offers_a_start() {
        let page = dashboard_page(None).unwrap();
        assert!(page.contains("<title>Poker Trainer</title>"));
        assert!(
            page.contains(r#"href="/play">Start tournament</a>"#),
            "the start button opens the table: {page}"
        );
        assert!(
            !page.contains("Resume tournament"),
            "no resume offer without an active tournament"
        );
        assert!(
            page.contains(r#"href="/tournaments""#),
            "the dashboard links to the history"
        );
    }

    #[test]
    fn dashboard_with_an_active_tournament_offers_only_a_resume() {
        let page = dashboard_page(active_summary(true).as_ref()).unwrap();
        assert!(page.contains("Tournament in progress"));
        assert!(
            page.contains(r#"href="/play">Resume tournament</a>"#),
            "resume points at the table: {page}"
        );
        assert!(
            !page.contains("Start tournament"),
            "a new tournament cannot start while one is active"
        );
        assert!(page.contains("Hand</span><b>#12</b>"));
        assert!(page.contains("Street</span><b>Flop</b>"));
        assert!(page.contains("Blinds</span><b>10/20</b>"));
        assert!(page.contains("Your stack</span><b>460</b>"));
        assert!(page.contains("Opponents left</span><b>2</b>"));
        assert!(page.contains("Actions played</span><b>41</b>"));
        assert!(page.contains("2026-08-24T10:00:00Z"));
    }

    #[test]
    fn dashboard_escapes_stored_strings() {
        let mut summary = active_summary(true).unwrap();
        summary.started = r#"<script>"evil"</script>"#.to_string();
        let page = dashboard_page(Some(&summary)).unwrap();
        assert!(!page.contains(r#"<script>"evil""#));
        // Askama's escaper emits numeric entities (`&#60;`) where the old
        // hand-rolled `escape()` emitted named ones (`&lt;`). Both are
        // identical to a browser; unlike the old helper, Askama also escapes
        // `'` and cannot be forgotten at a call site.
        assert!(page.contains("&#60;script&#62;"));
        assert!(page.contains("&#34;evil&#34;"));
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
            result: None,
            final_stack: None,
        }
    }

    #[test]
    fn tournaments_page_has_an_empty_state() {
        let empty: Vec<(SessionSummary, Vec<ChartPoint>)> = Vec::new();
        let page = tournaments_page(&empty, 1, 1).unwrap();
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
        let page = tournaments_page(&sessions, 1, 1).unwrap();
        assert!(page.contains(r#"data-tournament-id="7""#));
        assert!(page.contains("Tournament #7"));
        assert!(
            page.contains(r#"href="/tournaments/7""#),
            "each card links to its detail page: {page}"
        );
        assert!(page.contains("3 hands · 3 actions · avg EV loss 12.50 BB"));
        assert!(page.contains("2026-08-01T10:00:00Z → 2026-08-01T10:05:00Z"));
        assert!(page.contains("Tournament #42"));
        assert!(
            page.contains(r#"data-points='[[1,0.0],[2,30.0],[3,12.5]]'"#),
            "decimated datasets are embedded for the client chart"
        );
        assert!(page.contains(r#"data-points='[[1,4.5],[2,0.0]]'"#));
        assert!(page.contains(r#"src="/assets/chart.js"#));
    }

    #[test]
    fn tournaments_page_escapes_database_strings() {
        let sessions = vec![(
            summary(1, r#"<script>"evil"</script>"#, "end", 1, 1, 0.0),
            vec![(1, 0.0)],
        )];
        let page = tournaments_page(&sessions, 1, 1).unwrap();
        assert!(!page.contains(r#"<script>"evil""#));
        assert!(page.contains("&#60;script&#62;"));
    }

    #[test]
    fn tournaments_page_renders_pagination_controls() {
        let empty: Vec<(SessionSummary, Vec<ChartPoint>)> = Vec::new();

        let first = tournaments_page(&empty, 1, 1).unwrap();
        assert!(first.contains(r#"class="pt-pagination""#));
        assert!(first.contains("Page 1 of 1"));
        assert!(
            first.contains(r#"<span class="pt-pagination-link is-disabled">← Newer</span>"#),
            "the first page has no newer page: {first}"
        );
        assert!(
            first.contains(r#"<span class="pt-pagination-link is-disabled">Older →</span>"#),
            "a single page has no older page: {first}"
        );

        let middle = tournaments_page(&empty, 2, 3).unwrap();
        assert!(middle.contains("Page 2 of 3"));
        assert!(
            middle.contains(r#"href="/tournaments?page=1"#),
            "the newer link points at the previous page: {middle}"
        );
        assert!(
            middle.contains(r#"href="/tournaments?page=3"#),
            "the older link points at the next page: {middle}"
        );

        let last = tournaments_page(&empty, 3, 3).unwrap();
        assert!(last.contains(r#"href="/tournaments?page=2"#));
        assert!(
            last.contains(r#"<span class="pt-pagination-link is-disabled">Older →</span>"#),
            "the last page has no older page: {last}"
        );
    }

    fn detail(id: i32, result: Option<&str>, final_stack: Option<i32>) -> TournamentDetail {
        TournamentDetail {
            summary: SessionSummary {
                id,
                started: "2026-08-01T10:00:00Z".to_string(),
                ended: "2026-08-01T10:05:00Z".to_string(),
                actions: 4,
                hands: 3,
                avg_ev_loss: 2.5,
                result: result.map(str::to_string),
                final_stack,
            },
            hands: 3,
            hands_won: 2,
            hands_lost: 1,
            all_ins: 1,
            all_in_pct: 33.3,
            total_ev_loss: 10.0,
            max_ev_loss: 6.0,
            points: vec![(1, 0.0), (2, 6.0), (3, 4.0)],
        }
    }

    #[test]
    fn tournament_detail_page_renders_the_stat_grid() {
        let page = tournament_detail_page(&detail(7, Some("WIN"), Some(1500))).unwrap();
        assert!(page.contains("<title>Poker Trainer — Tournament #7</title>"));
        assert!(page.contains(r#"class="pt-result-badge win">WIN</span>"#));
        assert!(page.contains("Final stack: 1500 chips"));
        assert!(page.contains("<span>Hands</span><b>3</b>"));
        assert!(page.contains("<span>Hands won</span><b>2</b>"));
        assert!(page.contains("<span>Hands lost</span><b>1</b>"));
        assert!(page.contains("<span>Win rate</span><b>67%</b>"));
        assert!(page.contains("<span>All-ins</span><b>1</b>"));
        assert!(page.contains("<span>All-in %</span><b>33%</b>"));
        assert!(page.contains("<span>Avg EV loss</span><b>2.50 BB</b>"));
        assert!(page.contains("<span>Total EV lost</span><b>10.00 BB</b>"));
        assert!(page.contains("<span>Biggest blunder</span><b>6.00 BB</b>"));
        assert!(page.contains(r#"data-points='[[1,0.0],[2,6.0],[3,4.0]]'"#));
    }

    #[test]
    fn tournament_detail_page_marks_losses_and_missing_results() {
        let loss = tournament_detail_page(&detail(9, Some("LOSS"), Some(0))).unwrap();
        assert!(loss.contains(r#"class="pt-result-badge loss">LOSS</span>"#));

        let unknown = tournament_detail_page(&detail(9, None, None)).unwrap();
        assert!(unknown.contains(r#"class="pt-result-badge">—</span>"#));
        assert!(unknown.contains("Final stack: —"));
    }

    #[test]
    fn eliminated_seats_render_out_and_no_cards() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(37)))
            .unwrap();
        state.set_eliminated(Seat::Opponent1, true);
        let fragment = table_fragment(&state, 1, 0, &[], &[]).unwrap();
        assert!(
            fragment.contains(r#"class="pt-flag bust">OUT</span>"#),
            "an eliminated seat is flagged OUT: {fragment}"
        );
        assert_eq!(
            fragment.matches(r#"class="pt-card back""#).count(),
            2,
            "only the non-eliminated opponent keeps hidden cards: {fragment}"
        );
    }

    #[test]
    fn table_fragment_reflects_the_current_state() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(31)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let fragment = table_fragment(&state, 3, 0, &["You check".to_string()], &[]).unwrap();
        assert!(fragment.contains(r#"id="table-state""#));
        assert!(fragment.contains("Hand #3"));
        assert!(fragment.contains("Blinds 10/20 · Preflop"));
        assert!(
            fragment.contains(r#"data-kind="call">Call<span class="amt">10</span>"#),
            "{fragment}"
        );
        assert!(fragment.contains(r#"data-kind="fold"#));
        assert!(
            fragment.contains(r#"class="pt-action-block" data-decision="h3-a0-preflop">"#)
                && fragment.contains(
                    r#"id="mcts-status" class="mcts-status status-bad">solver idle</div>"#
                ),
            "the solver depth badge sits in the action dock, tagged with the decision token, while the hero acts: {fragment}"
        );
        assert!(
            fragment.contains(r#"<div class="pt-dock-wait">"#),
            "the dock carries a wait hint shown until the depth badge turns green: {fragment}"
        );
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
        let fragment = table_fragment(&state, 1, 0, &[], &[]).unwrap();
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
        let fragment = table_fragment(&state, 1, 0, &[], &[]).unwrap();
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
            fragment.contains(r#"data-bucket="ALLIN" data-size="300""#),
            "the all-in chip is a live preset that drives the slider to the whole stack: {fragment}"
        );
    }

    #[test]
    fn action_panel_facing_a_raise_preflop_shows_pot_fractions_and_two_x() {
        // Hero calls, Opponent 1 raises to 100, Opponent 2 folds: the hero
        // faces 80 more into a pot of 140 with 280 behind.
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(38)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Raise(100)).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let fragment = table_fragment(&state, 1, 0, &[], &[]).unwrap();
        assert!(
            fragment.contains(r#"data-kind="call">Call<span class="amt">80</span>"#),
            "{fragment}"
        );
        assert!(
            fragment.contains(r#"data-kind="raise">Raise to<span class="amt">180</span>"#),
            "the default raise is 2x the call, clamped to the min-raise: {fragment}"
        );
        for pct in ["33%", "50%", "75%"] {
            assert!(
                fragment.contains(&format!(r#">{pct}</button>"#)),
                "pot-fraction chip {pct} is shown: {fragment}"
            );
        }
        assert!(
            fragment.contains(r#"data-bucket="ALLIN""#),
            "the pot-sized raise collapses into the all-in chip: {fragment}"
        );
    }

    #[test]
    fn action_panel_offers_a_direct_all_in_when_calling_costs_the_whole_stack() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(36)))
            .unwrap();
        state.apply_action(Action::Raise(300)).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let fragment = table_fragment(&state, 1, 0, &[], &[]).unwrap();
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
            0,
            &[],
            &[Sound::Deal, Sound::Chip, Sound::Fold, Sound::Win],
        )
        .unwrap();
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
        let waiting = table_fragment(&state, 1, 0, &[], &[]).unwrap();
        assert!(waiting.contains("Waiting for"));

        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        let finished = table_fragment(&state, 1, 0, &[], &[]).unwrap();
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
    fn the_decision_token_rides_only_on_hero_turn_fragments() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(35)))
            .unwrap();
        assert_ne!(state.to_act(), Seat::Hero);
        let waiting = table_fragment(&state, 1, 0, &[], &[]).unwrap();
        assert!(
            !waiting.contains("data-decision"),
            "no decision token while an opponent acts: {waiting}"
        );

        let mut hero_state = GameState::new(Seat::Hero, level());
        hero_state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(31)))
            .unwrap();
        hero_state.apply_action(Action::Call).unwrap();
        assert_eq!(hero_state.to_act(), Seat::Hero);
        let hero_turn = table_fragment(&hero_state, 1, 2, &[], &[]).unwrap();
        assert!(
            hero_turn.contains(r#"data-decision="h1-a2-preflop""#),
            "the decision token names hand, action count, and street: {hero_turn}",
        );

        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        let finished = table_fragment(&state, 1, 2, &[], &[]).unwrap();
        assert!(
            !finished.contains("data-decision"),
            "no decision token once the hand is over: {finished}"
        );
    }

    #[test]
    fn cards_render_with_four_deck_colors() {
        for (rank, suit, class) in [
            (Rank::Ace, Suit::Hearts, " red"),
            (Rank::King, Suit::Diamonds, " blue"),
            (Rank::Queen, Suit::Clubs, " green"),
            (Rank::Jack, Suit::Spades, ""),
        ] {
            let card = CardView::new(Card::new(rank, suit));
            assert_eq!(
                card.suit_class, class,
                "suit {suit:?} maps to `pt-card{class}`"
            );
            // `assets/app.js` reads the Debug spelling off `data-suit`.
            assert_eq!(card.suit_debug, format!("{suit:?}"));
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

        let fragment = table_fragment(&state, 2, 0, &[], &[]).unwrap();
        let oval_marker = fragment.find(r#"<div class="pt-oval""#).unwrap();
        let dock = fragment.find(r#"id="action-panel""#).unwrap();
        assert!(
            dock > oval_marker,
            "the action panel renders in its own block below the oval: {fragment}"
        );
        assert!(
            fragment.contains(r#"<div class="pt-action-block" data-decision="h2-a0-preflop"><div id="mcts-status""#)
                && dock > fragment.find(r#"id="mcts-status""#).unwrap(),
            "the depth badge leads the action block and the panel stays under the oval: {fragment}"
        );
    }

    #[test]
    fn seats_render_without_avatar_icons() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(39)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        let fragment = table_fragment(&state, 4, 0, &[], &[]).unwrap();
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

        let fragment = table_fragment(&state, 1, 0, &log, &[]).unwrap();
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
        let fragment = table_fragment(&state, 5, 0, &[], &[]).unwrap();
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
        let raised = table_fragment(&state, 5, 0, &[], &[]).unwrap();
        assert!(
            raised.contains(r#"class="pt-bet">60</div>"#),
            "the raise amount shows in front of the raiser: {raised}"
        );

        // Close the betting round: street bets are swept into the pot pill.
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.advance_street(&mut deck).unwrap();
        let settled = table_fragment(&state, 5, 0, &[], &[]).unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        let fragment = tactical_overlay_fragment(7, &decision, false, &[], 20, 20, 500).unwrap();
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
        let fragment = tactical_overlay_fragment(7, &decision, false, &[], 20, 20, 500).unwrap();
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
        let fragment = tactical_overlay_fragment(
            7,
            &sample_analysis(),
            false,
            &sample_opponents(),
            20,
            20,
            500,
        )
        .unwrap();
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
        let fragment =
            tactical_overlay_fragment(7, &sample_analysis(), false, &opponents, 20, 20, 500)
                .unwrap();
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

        let fragment = table_fragment(&state, 1, 0, &[], &[]).unwrap();
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

    /// An uncalled bet portion handed back after a showdown is not a win: the
    /// true winner carries the WIN badge and the returned seat does not.
    #[test]
    fn returned_uncalled_bets_do_not_mark_a_winner() {
        let snapshot = crate::snapshot::StateSnapshot {
            stacks: [710, 300, 315],
            button: 0,
            blind_small: 10,
            blind_big: 20,
            street: 3,
            board: vec![
                "3c".into(),
                "Ac".into(),
                "9d".into(),
                "3h".into(),
                "8d".into(),
            ],
            hole_cards: vec![
                ["As".into(), "2s".into()],
                ["7h".into(), "7d".into()],
                ["Qs".into(), "6d".into()],
            ],
            revealed: [true, true, true],
            street_contrib: [0, 0, 0],
            total_contrib: [210, 10, 215],
            current_bet: 0,
            min_raise: 20,
            last_full_raise: None,
            acted: [false, false, false],
            folded: [false, true, false],
            all_in: [true, false, true],
            eliminated: [false, false, false],
            to_act: 0,
            hand_over: true,
            hand_result: Some(crate::snapshot::HandResultSnapshot {
                reason: "showdown".into(),
                awards: vec![(0, 430)],
                returns: vec![(2, 5)],
            }),
        };
        let state = GameState::from_snapshot(&snapshot).unwrap();
        let fragment = table_fragment(&state, 1, 0, &[], &[]).unwrap();
        assert!(
            fragment.contains(r#"class="pt-win"><b>WIN</b><span>+430</span>"#),
            "the real winner carries the WIN badge: {fragment}"
        );
        assert!(
            !fragment.contains("+5</span>"),
            "an uncalled return is not rendered as a win: {fragment}"
        );
        assert_eq!(fragment.matches("pt-winner").count(), 1);
    }

    // ------------------------------------------------------- hand history

    fn hh_stats() -> crate::hh::OverallStats {
        crate::hh::OverallStats {
            tournaments: 4,
            tournaments_won: 1,
            hands: 40,
            hands_won: 23,
            showdowns: 30,
            all_ins: 12,
            buy_in_cents: 100,
            prize_cents: 75,
            invested: 1200,
            collected: 1350,
            net_chips: 150,
        }
    }

    fn hh_listing() -> Vec<crate::hh::TournamentListing> {
        vec![crate::hh::TournamentListing {
            tournament: crate::hh::TournamentSummary {
                id: "307865587".to_string(),
                name: "Spin&Gold #7".to_string(),
                game_type: Some("Hold'em No Limit".to_string()),
                started: "2026-08-21 15:03:37".to_string(),
                finished: Some("2026-08-21 15:07:44".to_string()),
                buy_in_cents: Some(25),
                prize_cents: Some(75),
                place: Some(1),
                entrants: Some(3),
            },
            hands: 10,
            hands_won: 6,
            all_ins: 4,
            showdowns: 7,
            net_chips: 200,
        }]
    }

    #[test]
    fn history_page_renders_the_scan_button_stats_and_listing() {
        let page = history_page(&hh_stats(), &hh_listing(), None).unwrap();
        assert!(page.contains("<title>Poker Trainer — Hand history</title>"));
        assert!(
            page.contains(r#"action="/history/scan""#)
                && page.contains("Scan for new hand histories"),
            "the scan form posts to the importer: {page}"
        );
        assert!(page.contains("<span>Tournaments</span><b>4</b>"));
        assert!(page.contains("Net profit"));
        assert!(page.contains("$0.50"), "{page}");
        assert!(page.contains("Hands won</span><b>23 (57.5%)</b>"));
        assert!(
            page.contains(r#"href="/history/tournaments/307865587""#),
            "each tournament row links to its detail: {page}"
        );
        assert!(page.contains("3 players"));
        assert!(page.contains("$0.75"), "{page}");
        assert!(page.contains("Win %</th>"));
    }

    #[test]
    fn history_page_has_an_empty_state_without_tournaments() {
        let page = history_page(&hh_stats(), &[], None).unwrap();
        assert!(page.contains("No imported hand histories yet"));
        assert!(!page.contains("pt-hh-table"));
    }

    #[test]
    fn history_page_escapes_stored_strings() {
        let mut stats = hh_stats();
        stats.net_chips = -15;
        let mut listing = hh_listing();
        listing[0].tournament.name = r#"<script>"evil"</script>"#.to_string();
        let page = history_page(&stats, &listing, None).unwrap();
        assert!(!page.contains(r#"<script>"evil""#));
        assert!(page.contains("&#60;script&#62;"));
    }

    fn hh_outcome() -> crate::hh::ImportOutcome {
        crate::hh::ImportOutcome {
            zips: 2,
            files: 3,
            hands_parsed: 20,
            tournaments_parsed: 2,
            hands_new: 12,
            hands_skipped: 8,
            tournaments_new: 1,
            failures: vec!["bad.zip/entry.txt: unreadable".to_string()],
            new_stats: crate::hh::NewHandStats {
                hands: 12,
                won: 7,
                lost: 5,
                win_ratio: 58.3,
                all_ins: 3,
                showdowns: 9,
                invested: 400,
                collected: 520,
                net_chips: 120,
                tournaments: 1,
            },
        }
    }

    #[test]
    fn scan_result_page_counts_only_the_new_hands() {
        let page = history_scan_result_page(&hh_outcome()).unwrap();
        assert!(page.contains("<title>Poker Trainer — Scan results</title>"));
        assert!(page.contains("<span>New hands</span><b>12</b>"));
        assert!(page.contains("<span>Already imported</span><b>8</b>"));
        assert!(page.contains("<span>Won</span><b>7</b>"));
        assert!(page.contains("<span>Win ratio</span><b>58.3%</b>"));
        assert!(page.contains("<span>Chips won/lost</span><b>+120</b>"));
        assert!(page.contains("bad.zip/entry.txt: unreadable"));
        assert!(
            page.contains("only the hands imported by this scan"),
            "{page}"
        );
        assert!(page.contains(r#"href="/history""#));
    }

    #[test]
    fn scan_result_page_shows_a_clean_run_and_escapes_failures() {
        let mut outcome = hh_outcome();
        outcome.failures = vec![r#"<script>"bad"</script>"#.to_string()];
        let page = history_scan_result_page(&outcome).unwrap();
        assert!(!page.contains(r#"<script>"bad""#));

        let mut clean = hh_outcome();
        clean.failures = Vec::new();
        let page = history_scan_result_page(&clean).unwrap();
        assert!(page.contains("No problems were found."));
        assert!(page.contains("0"));
    }

    fn hh_detail() -> crate::hh::TournamentDetail {
        crate::hh::TournamentDetail {
            listing: hh_listing().remove(0),
            hands: vec![
                crate::hh::HandRow {
                    hand_id: "SG1".to_string(),
                    played_at: "2026-08-21 15:07:44".to_string(),
                    sb: 20,
                    bb: 40,
                    position: "BB".to_string(),
                    table_size: 2,
                    hero_stack: Some(525),
                    hero_cards: Some("As Kh".to_string()),
                    all_in: true,
                    showdown: true,
                    hero_won: true,
                    invested: 375,
                    collected: 750,
                    net: 375,
                    board: Some("Jd 3c 8c Qd 7s".to_string()),
                },
                crate::hh::HandRow {
                    hand_id: "SG2".to_string(),
                    played_at: "2026-08-21 15:03:37".to_string(),
                    sb: 10,
                    bb: 20,
                    position: "SB".to_string(),
                    table_size: 2,
                    hero_stack: None,
                    hero_cards: None,
                    all_in: false,
                    showdown: false,
                    hero_won: false,
                    invested: 10,
                    collected: 0,
                    net: -10,
                    board: None,
                },
            ],
        }
    }

    #[test]
    fn history_tournament_detail_page_renders_stats_and_hands() {
        let page = history_tournament_detail_page(&hh_detail()).unwrap();
        assert!(page.contains("<title>Poker Trainer — Spin&#38;Gold #7</title>"));
        assert!(page.contains(r#"class="pt-result-badge win">WIN</span>"#));
        assert!(page.contains("Buy-in $0.25"));
        assert!(page.contains("Prize $0.75"));
        assert!(page.contains("Profit $0.50"));
        assert!(page.contains("<span>Hands</span><b>10</b>"));
        assert!(page.contains("<span>Win rate</span><b>60%</b>"));
        assert!(page.contains("<span>Net chips</span><b>+200</b>"));
        assert!(page.contains("As Kh"));
        assert!(page.contains("Jd 3c 8c Qd 7s"));
        assert!(page.contains("2026-08-21 15:07:44"));
        assert!(page.contains("+375"));
        assert!(page.contains("-10"));
    }

    #[test]
    fn history_tournament_detail_page_handles_unknown_money_and_hands() {
        let mut detail = hh_detail();
        detail.listing.tournament.buy_in_cents = None;
        detail.listing.tournament.prize_cents = None;
        detail.listing.tournament.place = None;
        detail.hands.clear();
        let page = history_tournament_detail_page(&detail).unwrap();
        assert!(page.contains(r#"class="pt-result-badge">—</span>"#));
        assert!(page.contains("Buy-in —"));
        assert!(page.contains("No hands stored for this tournament yet"));
    }

    #[test]
    fn small_helpers_round_and_sign_correctly() {
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(signed(300), "+300");
        assert_eq!(signed(-150), "-150");
        assert_eq!(signed(0), "0");
        assert_eq!(pct(0, 0), 0.0);
        assert_eq!(pct(5, 10), 50.0);
        assert_eq!(round1(66.666), 66.7);
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "—");
    }

    // ------------------------------------------------------- opponent skill

    fn template_fixture() -> crate::opponent_analysis::DrillTemplate {
        crate::opponent_analysis::DrillTemplate {
            label: "Imported field (132 decisions)".to_string(),
            skill: 0.62,
            avg_ev_loss_bb: 0.4,
            decisions: 132,
        }
    }

    #[test]
    fn play_page_shows_the_hero_vs_field_skill_chip() {
        let page = play_page(Some(0.71), Some(0.62)).unwrap();
        assert!(page.contains("pt-skill-chip"), "{page}");
        assert!(
            page.contains("You <b>0.71</b> · Bots <b>0.62</b>"),
            "{page}"
        );

        let missing = play_page(None, Some(0.62)).unwrap();
        assert!(
            missing.contains("You <b>—</b> · Bots <b>0.62</b>"),
            "{missing}"
        );

        let none = play_page(None, None).unwrap();
        assert!(!none.contains("pt-skill-chip"), "no store, no chip: {none}");
    }

    #[test]
    fn history_page_offers_the_analyzer_and_the_current_template() {
        let page = history_page(&hh_stats(), &hh_listing(), None).unwrap();
        assert!(
            page.contains(r#"action="/history/analyze-opponents""#)
                && page.contains("Analyze imported opponents"),
            "the analyzer button sits next to the scan button: {page}"
        );
        assert!(
            !page.contains("pt-template-chip"),
            "no template means no chip: {page}"
        );

        let template = template_fixture();
        let page = history_page(&hh_stats(), &[], Some(&template)).unwrap();
        assert!(page.contains("Bots trained on: Imported field (132 decisions)"));
        assert!(page.contains("skill <b>0.62</b>"), "{page}");
        assert!(page.contains(r#"action="/history/clear-template""#));
    }

    fn report_fixture() -> crate::opponent_analysis::FieldReport {
        crate::opponent_analysis::FieldReport {
            hands_total: 100,
            hands_graded: 95,
            hands_failed: 5,
            decisions: 212,
            avg_ev_loss_bb: 0.4,
            skill: 0.62,
            players: vec![crate::opponent_analysis::PlayerRow {
                name: "14c11a2a".to_string(),
                decisions: 120,
                avg_ev_loss_bb: 0.3,
            }],
            problems: vec!["hand SG1: engine call amount 40 differs from the real 20".to_string()],
        }
    }

    #[test]
    fn analysis_page_shell_polls_the_status_endpoint() {
        let page = analysis_page().unwrap();
        assert!(page.contains("<title>Poker Trainer — Opponent analysis</title>"));
        assert!(page.contains(r#"src="/assets/analysis.js"#));
        assert!(page.contains("id=\"analysis-status\""), "{page}");
    }

    #[test]
    fn analysis_status_html_covers_every_job_state() {
        use crate::opponent_analysis::{FieldReport, JobState};

        let idle = analysis_status_html(&JobState::Idle).unwrap();
        assert!(idle.contains("Analyze imported opponents"), "{idle}");

        let running = analysis_status_html(&JobState::Running {
            hands_done: 30,
            hands_total: 100,
        })
        .unwrap();
        assert!(running.contains("hand 30 of 100"), "{running}");

        let done = analysis_status_html(&JobState::Done(report_fixture())).unwrap();
        assert!(done.contains("Hands in window</span><b>100</b>"), "{done}");
        assert!(done.contains("Field skill</span><b>0.62</b>"), "{done}");
        assert!(done.contains("14c11a2a"), "{done}");
        assert!(done.contains("0.300"), "{done}");
        assert!(done.contains(r#"action="/history/save-template""#));
        assert!(done.contains("engine call amount 40 differs"), "{done}");

        // A report without graded decisions never offers a save action.
        let empty = analysis_status_html(&JobState::Done(FieldReport::empty())).unwrap();
        assert!(!empty.contains("save-template"), "{empty}");
    }
}
