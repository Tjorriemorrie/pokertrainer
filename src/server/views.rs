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

#[cfg(test)]
fn legacy_dashboard_page(active: Option<&ActiveSummary>) -> String {
    let mut html = String::from(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer</title>
<link rel="stylesheet" href="/assets/style.css?v=13">
</head>
<body class="pt-body">
<header class="pt-topwrap">
  <div class="pt-brand">Poker Trainer</div>
  <a href="/tournaments" class="pt-link">Tournament history</a>
  <a href="/history" class="pt-link">Hand history</a>
</header>
<main class="pt-main pt-dashboard">
"#,
    );

    match active {
        Some(summary) => {
            html.push_str(&format!(
                r##"<section class="pt-dash-card">
  <h1 class="pt-dash-title">Tournament in progress</h1>
  <p class="pt-dash-meta">Started {}</p>
  <div class="pt-stat-grid">
    <div class="pt-stat-card"><span>Hand</span><b>#{}</b></div>
    <div class="pt-stat-card"><span>Street</span><b>{}</b></div>
    <div class="pt-stat-card"><span>Blinds</span><b>{}/{}</b></div>
    <div class="pt-stat-card"><span>Your stack</span><b>{}</b></div>
    <div class="pt-stat-card"><span>Opponents left</span><b>{}</b></div>
    <div class="pt-stat-card"><span>Actions played</span><b>{}</b></div>
  </div>
  <p class="pt-dash-note">Resume continues the exact hand — street, bets, board, and stacks are all restored.</p>
  <a class="action-btn pt-confirm pt-dash-action" href="/play">Resume tournament</a>
  <p class="pt-dash-note">A new tournament becomes available once this one ends — win it, lose it, or
  finish the table (which counts as giving up).</p>
</section>"##,
                escape(&summary.started),
                summary.hand_no,
                escape(&summary.street.to_string()),
                summary.blind_small,
                summary.blind_big,
                summary.hero_stack,
                summary.active_opponents,
                summary.actions,
            ));
        }
        None => {
            html.push_str(
                r#"<section class="pt-dash-card">
  <h1 class="pt-dash-title">Spin &amp; Gold — 3-Max</h1>
  <p class="pt-dash-meta">One table at a time: play it to the end or finish the table to give up.</p>
  <a class="action-btn pt-confirm pt-dash-action" href="/play">Start tournament</a>
</section>"#,
            );
        }
    }
    html.push_str(
        r#"</main>
</body>
</html>
"#,
    );
    html
}

/// The full table shell page: GGPoker-dark skin, top-bar lifetime EV chart (with
/// the hero-vs-field skill chip beside it), the table controls (finish,
/// tournament history, sound toggle), the table column docked top-left, and the
/// coach-feedback panel beside it (never covering the table).
pub fn play_page(you: Option<f64>, bots: Option<f64>) -> String {
    let skill_chip = skill_chip(you, bots);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer</title>
<link rel="stylesheet" href="/assets/style.css?v=13">
</head>
<body class="pt-body">
  <header class="pt-topwrap">
    <a href="/" class="pt-link">Dashboard</a>
    <div class="pt-brand">Poker Trainer</div>
    <canvas id="ev-chart" width="1200" height="48" class="ev-chart"></canvas>
    {skill_chip}
    <div id="ws-status" class="status-wait">connecting…</div>
    <button id="sound-toggle" class="pt-icon-btn" type="button" title="Toggle table sounds">🔊</button>
    <a href="/tournaments" class="pt-link">Tournament history</a>
    <a href="/history" class="pt-link">Hand history</a>
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
  <div id="tournament-modal" class="pt-modal" hidden>
    <div class="pt-modal-card">
      <h2 id="tournament-modal-title" class="pt-modal-title">Tournament over</h2>
      <p id="tournament-modal-body" class="pt-modal-body"></p>
      <button id="tournament-modal-continue" class="action-btn pt-confirm" type="button">Continue</button>
    </div>
  </div>
  <script src="/assets/app.js?v=7"></script>
</body>
</html>"#,
        skill_chip = if skill_chip.is_empty() {
            String::new()
        } else {
            format!("    {skill_chip}\n")
        }
    )
}

/// The top-bar chip comparing the hero's lifetime skill against the bot
/// template's field skill, on the same 0..1 scale. Empty when the app has no
/// analytics store to derive either number from.
fn skill_chip(you: Option<f64>, bots: Option<f64>) -> String {
    let (you, bots) = match (you, bots) {
        (None, None) => return String::new(),
        (you, bots) => (format_skill(you), format_skill(bots)),
    };
    format!(
        r#"<div class="pt-skill-chip" title="Skill on a 0..1 scale: how close your decisions average to the solver vs the imported opponents both bots play like. Generate the field skill under Hand history → Analyze imported opponents.">You <b>{you}</b> · Bots <b>{bots}</b></div>"#
    )
}

/// Formats one skill value for the header chip: two decimals, or an em dash
/// when the value does not exist yet.
fn format_skill(skill: Option<f64>) -> String {
    skill
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "—".to_string())
}

/// The finished-tournament history page: a paginated listing (newest first)
/// of one server-rendered card per finished session whose decimated EV
/// dataset is drawn client-side with the same canvas style as the live
/// top-bar chart. `page`/`pages` drive the Newer/Older navigation.
pub fn tournaments_page(
    sessions: &[(SessionSummary, Vec<ChartPoint>)],
    page: u32,
    pages: u32,
) -> String {
    let mut html = String::from(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer — Tournaments</title>
<link rel="stylesheet" href="/assets/style.css?v=13">
</head>
<body class="pt-body">
<header class="pt-topwrap">
  <h1 class="pt-page-title">Tournaments</h1>
  <a href="/" class="pt-link">Dashboard</a>
</header>
<main class="pt-main">
"#,
    );

    html.push_str(&pagination_nav(page, pages));

    if sessions.is_empty() {
        html.push_str(
            r#"<div class="pt-empty">No finished tournaments yet — play a table and finish it (or just close the tab) to see its EV history here.</div>"#,
        );
    } else {
        for (summary, points) in sessions {
            let dataset = serde_json::to_string(points).unwrap_or_else(|_| "[]".to_string());
            let result_badge = match summary.result.as_deref() {
                Some("WIN") => r#"<span class="pt-result-badge win">WIN</span>"#,
                Some("LOSS") => r#"<span class="pt-result-badge loss">LOSS</span>"#,
                _ => "",
            };
            html.push_str(&format!(
                r#"<section class="pt-tournament" data-tournament-id="{}">
  <div class="pt-tournament-head">
    <a class="pt-tournament-title" href="/tournaments/{}">Tournament #{}</a>
    {result_badge}
    <span class="pt-tournament-meta">{} → {}</span>
    <span class="pt-tournament-meta">{} hands · {} actions · avg EV loss {:.2} BB</span>
  </div>
  <canvas class="ev-chart" width="1200" height="48" data-points='{}'></canvas>
</section>"#,
                summary.id,
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

/// The Newer/Older pagination bar of the tournaments page: links navigate
/// between pages while the current page's position is spelled out. Disabled
/// edges render as inert spans.
fn pagination_nav(page: u32, pages: u32) -> String {
    let newer = if page > 1 {
        format!(
            r#"<a class="pt-pagination-link" href="/tournaments?page={}">← Newer</a>"#,
            page - 1
        )
    } else {
        r#"<span class="pt-pagination-link is-disabled">← Newer</span>"#.to_string()
    };
    let older = if page < pages {
        format!(
            r#"<a class="pt-pagination-link" href="/tournaments?page={}">Older →</a>"#,
            page + 1
        )
    } else {
        r#"<span class="pt-pagination-link is-disabled">Older →</span>"#.to_string()
    };
    format!(
        r#"<nav class="pt-pagination">{newer}<span class="pt-pagination-page">Page {page} of {pages}</span>{older}</nav>"#
    )
}

/// The single-tournament detail page: the outcome, hand-level aggregates
/// (hands, wins, losses, all-in frequency), EV stats, and the decimated
/// action-EV chart.
pub fn tournament_detail_page(detail: &TournamentDetail) -> String {
    let summary = &detail.summary;
    let dataset = serde_json::to_string(&detail.points).unwrap_or_else(|_| "[]".to_string());
    let win_rate = if detail.hands > 0 {
        detail.hands_won as f64 * 100.0 / detail.hands as f64
    } else {
        0.0
    };

    let result_badge = match summary.result.as_deref() {
        Some("WIN") => r#"<span class="pt-result-badge win">WIN</span>"#,
        Some("LOSS") => r#"<span class="pt-result-badge loss">LOSS</span>"#,
        _ => r#"<span class="pt-result-badge">—</span>"#,
    };
    let final_stack = summary
        .final_stack
        .map(|stack| format!("{stack} chips"))
        .unwrap_or_else(|| "—".to_string());

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer — Tournament #{}</title>
<link rel="stylesheet" href="/assets/style.css?v=13">
</head>
<body class="pt-body">
<header class="pt-topwrap">
  <h1 class="pt-page-title">Tournament #{}</h1>
  <a href="/tournaments" class="pt-link">All tournaments</a>
  <a href="/" class="pt-link">Dashboard</a>
</header>
<main class="pt-main">
  <section class="pt-detail">
    <div class="pt-detail-head">
      {result_badge}
      <span class="pt-detail-meta">{} → {}</span>
      <span class="pt-detail-meta">Final stack: {}</span>
    </div>
    <div class="pt-stat-grid">
      <div class="pt-stat-card"><span>Hands</span><b>{}</b></div>
      <div class="pt-stat-card"><span>Hands won</span><b>{}</b></div>
      <div class="pt-stat-card"><span>Hands lost</span><b>{}</b></div>
      <div class="pt-stat-card"><span>Win rate</span><b>{:.0}%</b></div>
      <div class="pt-stat-card"><span>All-ins</span><b>{}</b></div>
      <div class="pt-stat-card"><span>All-in %</span><b>{:.0}%</b></div>
      <div class="pt-stat-card"><span>Avg EV loss</span><b>{:.2} BB</b></div>
      <div class="pt-stat-card"><span>Total EV lost</span><b>{:.2} BB</b></div>
      <div class="pt-stat-card"><span>Biggest blunder</span><b>{:.2} BB</b></div>
    </div>
    <canvas class="ev-chart" width="1200" height="48" data-points='{}'></canvas>
  </section>
</main>
<script>
(() => {{
  "use strict";
  document.querySelectorAll("canvas[data-points]").forEach((canvas) => {{
    const ctx = canvas.getContext("2d");
    const values = JSON.parse(canvas.dataset.points || "[]").map((point) => point[1]);
    if (values.length < 2) return;
    const max = Math.max(1, ...values);
    const step = canvas.width / (values.length - 1);
    ctx.beginPath();
    values.forEach((value, i) => {{
      const x = i * step;
      const y = canvas.height - (value / max) * (canvas.height - 6) - 3;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }});
    ctx.strokeStyle = "#f59e0b";
    ctx.lineWidth = 2;
    ctx.stroke();
  }});
}})();
</script>
</body>
</html>"##,
        summary.id,
        summary.id,
        escape(&summary.started),
        escape(&summary.ended),
        escape(&final_stack),
        detail.hands,
        detail.hands_won,
        detail.hands_lost,
        win_rate,
        detail.all_ins,
        detail.all_in_pct,
        summary.avg_ev_loss,
        detail.total_ev_loss,
        detail.max_ev_loss,
        dataset
    )
}

/// The GGPoker hand-history page: the scan trigger, the opponent-skill
/// analyzer entry and the current bot template, the lifetime
/// profit/win-rate aggregates, and one row per imported tournament (newest
/// first) linking to its hand-level detail page.
pub fn history_page(
    stats: &crate::hh::OverallStats,
    tournaments: &[crate::hh::TournamentListing],
    template: Option<&crate::opponent_analysis::DrillTemplate>,
) -> String {
    let tournament_win_ratio = pct(stats.tournaments_won, stats.tournaments);
    let hand_win_ratio = pct(stats.hands_won, stats.hands);
    let profit = stats.prize_cents - stats.buy_in_cents;

    let mut html = String::from(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer — Hand history</title>
<link rel="stylesheet" href="/assets/style.css?v=13">
</head>
<body class="pt-body">
<header class="pt-topwrap">
  <h1 class="pt-page-title">Hand history</h1>
  <a href="/" class="pt-link">Dashboard</a>
  <a href="/tournaments" class="pt-link">Tournament history</a>
</header>
<main class="pt-main">
"#,
    );

    let stat_card = |label: &str, value: &str| -> String {
        format!(
            r#"<div class="pt-stat-card"><span>{}</span><b>{}</b></div>"#,
            label,
            escape(value)
        )
    };

    let all_cards = format!(
        "{}{}{}{}{}{}{}{}{}{}",
        stat_card("Tournaments", &stats.tournaments.to_string()),
        stat_card(
            "Tournaments won",
            &format!(
                "{} ({}%)",
                stats.tournaments_won,
                round1(tournament_win_ratio)
            )
        ),
        stat_card("Hands", &stats.hands.to_string()),
        stat_card(
            "Hands won",
            &format!("{} ({}%)", stats.hands_won, round1(hand_win_ratio))
        ),
        stat_card("Net profit", &crate::hh::money(profit)),
        stat_card("Buy-ins", &crate::hh::money(stats.buy_in_cents)),
        stat_card("Prizes", &crate::hh::money(stats.prize_cents)),
        stat_card("All-ins", &stats.all_ins.to_string()),
        stat_card("Showdowns", &stats.showdowns.to_string()),
        stat_card("Net chips", &signed(stats.net_chips)),
    );

    html.push_str(&format!(
        r#"<section class="pt-detail">
  <div class="pt-detail-head">
    <h2 class="pt-hh-title">GGPoker hand histories</h2>
    <div class="pt-hh-actions">
      <form method="post" action="/history/scan">
        <button class="action-btn pt-confirm" type="submit">Scan for new hand histories</button>
      </form>
      <form method="post" action="/history/analyze-opponents">
        <button class="action-btn pt-confirm" type="submit">Analyze imported opponents</button>
      </form>
    </div>
  </div>
  <p class="pt-detail-meta">Reads the PokerCraft zip exports in the history/ folder and imports the hands into
  the database. Hands that were already imported are skipped, so re-scanning is always safe.</p>
  <p class="pt-detail-meta">Analyze imported opponents grades every opponent decision in your last
  1,000 imported hands against the solver and turns the average big-blind loss into the field skill
  level both bots play with.</p>
  {template_html}
  <div class="pt-stat-grid">
    {all_cards}
  </div>
</section>
<section class="pt-detail">
  <div class="pt-detail-head"><h2 class="pt-hh-title">Tournaments</h2></div>
  {}
</section>
</main>
</body>
</html>"#,
        tournaments_html(tournaments),
        all_cards = all_cards,
        template_html = template_html(template),
    ));

    html
}

/// The chip showing the stored bot template (and its clear action) when one
/// exists.
fn template_html(template: Option<&crate::opponent_analysis::DrillTemplate>) -> String {
    let Some(template) = template else {
        return String::new();
    };
    format!(
        r#"<div class="pt-template-chip"><span>Bots trained on: {} — skill <b>{:.2}</b> ({:.2} BB lost/decision over {} decisions)</span>
  <form method="post" action="/history/clear-template">
    <button class="pt-link-btn" type="submit">Clear template</button>
  </form>
</div>"#,
        escape(&template.label),
        template.skill,
        template.avg_ev_loss_bb,
        template.decisions,
    )
}

/// The tournament listing table: one row per imported tournament, newest
/// first, with an empty state when nothing has been imported yet.
fn tournaments_html(tournaments: &[crate::hh::TournamentListing]) -> String {
    if tournaments.is_empty() {
        return r#"<div class="pt-empty">No imported hand histories yet — press <b>Scan for new hand histories</b> to read the zips in your history/ folder.</div>"#
            .to_string();
    }
    let mut html = String::from(
        r#"<table class="pt-hh-table">
<tr><th>Date</th><th>Tournament</th><th>Buy-in</th><th>Place</th><th>Prize</th><th>Profit</th><th>Hands</th><th>Won</th><th>Win %</th><th>Net chips</th></tr>
"#,
    );
    for row in tournaments {
        let tournament = &row.tournament;
        let buy_in = tournament
            .buy_in_cents
            .map(|cents| crate::hh::money(i64::from(cents)))
            .unwrap_or_else(|| "—".to_string());
        let prize = tournament
            .prize_cents
            .map(|cents| crate::hh::money(i64::from(cents)))
            .unwrap_or_else(|| "—".to_string());
        let profit = match (tournament.buy_in_cents, tournament.prize_cents) {
            (Some(buy), Some(prize)) => crate::hh::money(i64::from(prize) - i64::from(buy)),
            _ => "—".to_string(),
        };
        let profit_class = if profit.starts_with('-') {
            "pt-neg"
        } else if profit.starts_with('$') {
            "pt-pos"
        } else {
            ""
        };
        let place = tournament
            .place
            .map(ordinal)
            .unwrap_or_else(|| "—".to_string());
        let win_pct = pct(row.hands_won, row.hands);
        let date = tournament
            .finished
            .clone()
            .unwrap_or_else(|| tournament.started.clone());
        html.push_str(&format!(
            r#"<tr><td class="pt-hh-date">{}</td><td><a class="pt-link" href="/history/tournaments/{}">{}</a>{}<span class="pt-hh-sub">{}</span></td><td>{}</td><td>{}</td><td>{}</td><td class="{profit_class}">{}</td><td>{}</td><td>{}</td><td>{:.0}%</td><td class="{}">{}</td></tr>
"#,
            escape(&date),
            escape(&tournament.id),
            escape(&tournament.name),
            tournament
                .game_type
                .as_deref()
                .map_or_else(String::new, |game| format!(r#"<span class="pt-hh-sub">{} </span>"#, escape(game))),
            escape(
                &tournament
                    .entrants
                    .map_or_else(String::new, |n| format!("{n} players"))
            ),
            escape(&buy_in),
            place,
            escape(&prize),
            profit,
            row.hands,
            row.hands_won,
            win_pct,
            if row.net_chips < 0 { "pt-neg" } else { "pt-pos" },
            signed(row.net_chips),
        ));
    }
    html.push_str("</table>");
    html
}

/// The scan-results page: what the scan found and stored, plus statistics
/// over the newly imported hands only (already-imported hands add nothing).
pub fn history_scan_result_page(outcome: &crate::hh::ImportOutcome) -> String {
    let stats = &outcome.new_stats;
    let card = |label: &str, value: &str| -> String {
        format!(
            r#"<div class="pt-stat-card"><span>{}</span><b>{}</b></div>"#,
            label,
            escape(value)
        )
    };
    let failures = if outcome.failures.is_empty() {
        r#"<p class="pt-detail-meta">No problems were found.</p>"#.to_string()
    } else {
        let mut list = String::from(r#"<div class="pt-hh-failures">Skipped files:<ul>"#);
        for failure in &outcome.failures {
            list.push_str(&format!("<li>{}</li>", escape(failure)));
        }
        list.push_str("</ul></div>");
        list
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer — Scan results</title>
<link rel="stylesheet" href="/assets/style.css?v=13">
</head>
<body class="pt-body">
<header class="pt-topwrap">
  <h1 class="pt-page-title">Scan results</h1>
  <a href="/history" class="pt-link">Hand history</a>
  <a href="/" class="pt-link">Dashboard</a>
</header>
<main class="pt-main">
<section class="pt-detail">
  <div class="pt-detail-head"><h2 class="pt-hh-title">Import summary</h2></div>
  <div class="pt-stat-grid">
    {zips}{files}{parsed}{new}{skipped}{tournaments_new}
  </div>
  {failures}
</section>
<section class="pt-detail">
  <div class="pt-detail-head"><h2 class="pt-hh-title">Stats of the new hands</h2></div>
  <p class="pt-detail-meta">These numbers cover only the hands imported by this scan — hands that were already
  in the database are not counted.</p>
  <div class="pt-stat-grid">
    {hands}{won}{lost}{win_ratio}{all_ins}{showdowns}{invested}{collected}{net}{touched}
  </div>
  <a class="pt-link" href="/history">Back to hand history</a>
</section>
</main>
</body>
</html>"#,
        zips = card("ZIPs scanned", &outcome.zips.to_string()),
        files = card("Files read", &outcome.files.to_string()),
        parsed = card("Hands parsed", &outcome.hands_parsed.to_string()),
        new = card("New hands", &outcome.hands_new.to_string()),
        skipped = card("Already imported", &outcome.hands_skipped.to_string()),
        tournaments_new = card("New tournaments", &outcome.tournaments_new.to_string()),
        failures = failures,
        hands = card("Hands", &stats.hands.to_string()),
        won = card("Won", &stats.won.to_string()),
        lost = card("Lost", &stats.lost.to_string()),
        win_ratio = card("Win ratio", &format!("{}%", round1(stats.win_ratio))),
        all_ins = card("All-ins", &stats.all_ins.to_string()),
        showdowns = card("Showdowns", &stats.showdowns.to_string()),
        invested = card("Chips invested", &stats.invested.to_string()),
        collected = card("Chips collected", &stats.collected.to_string()),
        net = card("Chips won/lost", &signed(stats.net_chips)),
        touched = card("Tournaments", &stats.tournaments.to_string()),
    )
}

#[cfg(test)]
fn legacy_analysis_page() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer — Opponent analysis</title>
<link rel="stylesheet" href="/assets/style.css?v=13">
</head>
<body class="pt-body">
<header class="pt-topwrap">
  <h1 class="pt-page-title">Opponent analysis</h1>
  <a href="/history" class="pt-link">Hand history</a>
  <a href="/" class="pt-link">Dashboard</a>
</header>
<main class="pt-main">
<section class="pt-detail">
  <div class="pt-detail-head"><h2 class="pt-hh-title">Field skill</h2></div>
  <p class="pt-detail-meta">Every opponent decision in your last 1,000 imported hands is replayed and
  graded against the solver; the pooled average big-blind loss becomes the field skill. Save it as
  the template the two local bots play with, and compare it against your own lifetime skill in the
  table header.</p>
  <div id="analysis-status"><div class="pt-empty">Loading…</div></div>
</section>
</main>
<script>
(function () {
  var box = document.getElementById('analysis-status');
  function poll() {
    fetch('/history/analyze-status')
      .then(function (r) { return r.json(); })
      .then(function (status) {
        box.innerHTML = status.html;
        if (status.state === 'running') { setTimeout(poll, 1500); }
      })
      .catch(function () { box.innerHTML = '<div class="pt-empty">Status unavailable.</div>'; });
  }
  poll();
})();
</script>
</body>
</html>"#
        .to_string()
}

/// The status fragment swapped into the analysis page: idle nudge, live
/// progress, or the finished report with the save-template action.
pub fn analysis_status_html(status: &crate::opponent_analysis::JobState) -> String {
    use crate::opponent_analysis::JobState;

    match status {
        JobState::Idle => r#"<div class="pt-empty">No analysis running.
  <a class="pt-link" href="/history">Back to hand history</a> and press
  <b>Analyze imported opponents</b> to start.</div>"#
            .to_string(),
        JobState::Running {
            hands_done,
            hands_total,
        } => {
            let pct = if *hands_total == 0 {
                0.0
            } else {
                (*hands_done as f64 * 100.0) / (*hands_total as f64)
            };
            format!(
                r#"<div class="pt-status-running">Analyzing opponents — hand {} of {} ({pct:.0}%).
Grading one decision per possible opponent action is solver work, so a full pass can take a few minutes; already-analyzed hands are skipped.</div>"#,
                hands_done, hands_total,
            )
        }
        JobState::Done(report) => {
            let card = |label: &str, value: String| -> String {
                format!(
                    r#"<div class="pt-stat-card"><span>{}</span><b>{}</b></div>"#,
                    label,
                    escape(&value)
                )
            };
            let mut html = format!(
                r#"<div class="pt-stat-grid">
  {hands}{graded}{failed}{decisions}{avg}{skill}
</div>"#,
                hands = card("Hands in window", report.hands_total.to_string()),
                graded = card("Hands graded", report.hands_graded.to_string()),
                failed = card("Hands skipped", report.hands_failed.to_string()),
                decisions = card("Opponent decisions", report.decisions.to_string()),
                avg = card(
                    "Avg BB lost per decision",
                    format!("{:.3}", report.avg_ev_loss_bb)
                ),
                skill = card("Field skill", format!("{:.2}", report.skill)),
            );
            if !report.players.is_empty() {
                html.push_str(
                    r#"<table class="pt-hh-table">
<tr><th>Opponent</th><th>Decisions</th><th>Avg BB lost</th></tr>"#,
                );
                for player in &report.players {
                    html.push_str(&format!(
                        "<tr><td>{}</td><td>{}</td><td>{:.3}</td></tr>",
                        escape(&player.name),
                        player.decisions,
                        player.avg_ev_loss_bb
                    ));
                }
                html.push_str("</table>");
            }
            if !report.problems.is_empty() {
                html.push_str(r#"<div class="pt-hh-failures">Skipped hands:<ul>"#);
                for problem in &report.problems {
                    html.push_str(&format!("<li>{}</li>", escape(problem)));
                }
                html.push_str("</ul></div>");
            }
            if report.decisions > 0 {
                html.push_str(&format!(
                    r#"<form method="post" action="/history/save-template" class="pt-save-template">
  <button class="action-btn pt-confirm" type="submit">Use field skill {:.2} as the bot template</button>
</form>
<p class="pt-detail-meta">Both local bots will make their decisions at this skill level — press
<b>Start tournament</b> on the dashboard and the header chip shows how you compare.</p>"#,
                    report.skill
                ));
            }
            html
        }
    }
}

/// One imported tournament's detail page: the stored summary, its aggregate
/// stats, and every hand newest first.
pub fn history_tournament_detail_page(detail: &crate::hh::TournamentDetail) -> String {
    let listing = &detail.listing;
    let tournament = &listing.tournament;
    let buy_in = tournament
        .buy_in_cents
        .map(|cents| crate::hh::money(i64::from(cents)))
        .unwrap_or_else(|| "—".to_string());
    let prize = tournament
        .prize_cents
        .map(|cents| crate::hh::money(i64::from(cents)))
        .unwrap_or_else(|| "—".to_string());
    let profit = match (tournament.buy_in_cents, tournament.prize_cents) {
        (Some(buy), Some(prize)) => crate::hh::money(i64::from(prize) - i64::from(buy)),
        _ => "—".to_string(),
    };

    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Poker Trainer — {}</title>
<link rel="stylesheet" href="/assets/style.css?v=13">
</head>
<body class="pt-body">
<header class="pt-topwrap">
  <h1 class="pt-page-title">{} — Tournament #{}</h1>
  <a href="/history" class="pt-link">Hand history</a>
  <a href="/" class="pt-link">Dashboard</a>
</header>
<main class="pt-main">
<section class="pt-detail">
  <div class="pt-detail-head">
    {}
    <span class="pt-detail-meta">{} → {}</span>
    <span class="pt-detail-meta">Buy-in {}</span>
    <span class="pt-detail-meta">Prize {}</span>
    <span class="pt-detail-meta">Profit {}</span>
  </div>
  <div class="pt-stat-grid">
    <div class="pt-stat-card"><span>Hands</span><b>{}</b></div>
    <div class="pt-stat-card"><span>Hands won</span><b>{}</b></div>
    <div class="pt-stat-card"><span>Win rate</span><b>{:.0}%</b></div>
    <div class="pt-stat-card"><span>All-ins</span><b>{}</b></div>
    <div class="pt-stat-card"><span>Showdowns</span><b>{}</b></div>
    <div class="pt-stat-card"><span>Net chips</span><b>{}</b></div>
  </div>
  {hands_table}
</section>
</main>
</body>
</html>"#,
        escape(&tournament.name),
        escape(&tournament.name),
        escape(&tournament.id),
        result_badge(detail),
        escape(&tournament.started),
        escape(tournament.finished.as_deref().unwrap_or("?")),
        escape(&buy_in),
        escape(&prize),
        escape(&profit),
        listing.hands,
        listing.hands_won,
        pct(listing.hands_won, listing.hands),
        listing.all_ins,
        listing.showdowns,
        signed(listing.net_chips),
        hands_table = hands_table(&detail.hands),
    );
    html
}

/// WIN/LOSS badge for a tournament detail: WIN when the hero finished 1st.
fn result_badge(detail: &crate::hh::TournamentDetail) -> String {
    match detail.listing.tournament.place {
        Some(1) => r#"<span class="pt-result-badge win">WIN</span>"#.to_string(),
        Some(_) => r#"<span class="pt-result-badge loss">LOSS</span>"#.to_string(),
        None => r#"<span class="pt-result-badge">—</span>"#.to_string(),
    }
}

/// The hand-level table of one tournament: every hand newest first with its
/// chips result.
fn hands_table(hands: &[crate::hh::HandRow]) -> String {
    if hands.is_empty() {
        return r#"<div class="pt-empty">No hands stored for this tournament yet — scan the hand-history zips again and they will appear here.</div>"#
            .to_string();
    }
    let mut html = String::from(
        r#"<table class="pt-hh-table">
<tr><th>Time</th><th>Blinds</th><th>Pos</th><th>Table</th><th>Cards</th><th>All-in</th><th>Showdown</th><th>Invested</th><th>Collected</th><th>Result</th><th>Board</th></tr>
"#,
    );
    for hand in hands {
        let net_class = if hand.net < 0 { "pt-neg" } else { "pt-pos" };
        html.push_str(&format!(
            r#"<tr><td class="pt-hh-date">{}</td><td>{}/{}</td><td>{}</td><td>{}-max</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class="{net_class}">{}</td><td class="pt-hh-board">{}</td></tr>
"#,
            escape(&hand.played_at),
            hand.sb,
            hand.bb,
            escape(&hand.position),
            hand.table_size,
            escape(&hand.hero_cards.clone().unwrap_or_else(|| "—".to_string())),
            yes_no(hand.all_in),
            yes_no(hand.showdown),
            hand.invested,
            hand.collected,
            signed(i64::from(hand.net)),
            escape(&hand.board.clone().unwrap_or_default()),
        ));
    }
    html.push_str("</table>");
    html
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
/// own right-aligned block below the oval (never covering the hero's cards)
/// with the live solver-depth badge docked top-left of the same block, and
/// an always-visible action log docked to the left of the oval, exactly
/// as tall as the table. `sounds` carries the WebAudio cues the client
/// synthesizes for this update.
pub fn table_fragment(
    state: &GameState,
    hand_no: u64,
    action_no: u64,
    log: &[String],
    sounds: &[Sound],
) -> String {
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
        let decision = format!(
            "h{hand_no}-a{action_no}-{}",
            state.street().to_string().to_lowercase()
        );
        html.push_str(&format!(
            r#"<div class="pt-action-block" data-decision="{decision}">"#
        ));
        html.push_str(r#"<div id="mcts-status" class="mcts-status status-bad">solver idle</div>"#);
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
            None if state.eliminated(seat) => String::new(),
            None => r#"<span class="pt-card back"></span><span class="pt-card back"></span>"#
                .to_string(),
        },
    };

    let small_blind = state.small_blind_seat();
    let big_blind = state.big_blind_seat();
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
    } else if state.eliminated(seat) {
        r#"<span class="pt-flag bust">OUT</span>"#
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

/// The GGPoker-style bottom dock overlaid on the felt: sizing chips (chip
/// values only — no BB labels), a golden bet slider with fine-grain wheel
/// control, and the Fold / Check-Call / Bet-Raise buttons.
fn action_panel(state: &GameState) -> String {
    let legal = state.legal_actions();
    let level = state.blind_level();
    let mut html = String::from(r#"<div id="action-panel" class="pt-action-dock">"#);
    html.push_str(
        r#"<div class="pt-dock-wait">Simulating — actions unlock when the depth badge turns green.</div>"#,
    );

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
        let preflop = state.street() == Street::Preflop;
        let facing_raise =
            raising && to_call > 0 && (!preflop || state.current_bet() > level.big_blind);
        let pot_fractions = facing_raise || !preflop;

        html.push_str(r#"<div class="pt-bet-row">"#);
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
        let mut seen = Vec::new();
        for bucket in buckets {
            let raw = bucket.to_raise_to(state.total_pot(), to_call, level.big_blind, min, stack);
            let amount = raw.clamp(min, max);
            if amount >= stack || seen.contains(&amount) {
                continue;
            }
            seen.push(amount);
            let label = if pot_fractions {
                pot_percent_label(*bucket).to_string()
            } else {
                amount.to_string()
            };
            html.push_str(&format!(
                r#"<button type="button" class="pt-chip-size" data-bucket="{}" data-size="{amount}">{label}</button>"#,
                escape(bucket.label())
            ));
        }
        if legal.can_all_in {
            html.push_str(&format!(
                r#"<button type="button" class="pt-chip-size allin" data-bucket="ALLIN" data-size="{max}">All-in</button>"#
            ));
        }
        html.push_str("</div>");

        let default_bucket: BetSize = if facing_raise && preflop {
            BetSize::TwoX
        } else if preflop {
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
            r#"<button type="button" class="action-btn red" data-kind="fold">Fold</button>"#,
        );
    }
    if legal.can_check {
        html.push_str(
            r#"<button type="button" class="action-btn red" data-kind="check">Check</button>"#,
        );
    }
    if legal.can_call {
        html.push_str(&format!(
            r#"<button type="button" class="action-btn red" data-kind="call">Call<span class="amt">{}</span></button>"#,
            legal.call_amount
        ));
    }
    if !sizing && legal.can_all_in {
        html.push_str(&format!(
            r#"<button type="button" class="action-btn red" data-kind="all_in">All-in<span class="amt">{}</span></button>"#,
            state.stack(Seat::Hero)
        ));
    }
    if sizing {
        let red_label = if betting {
            format!(r#"Bet<span class="amt">{initial}</span>"#)
        } else {
            format!(r#"Raise to<span class="amt">{initial}</span>"#)
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
            r#"<span class="pt-opp-flag bust">OUT</span>"#
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
    fn play_page_shell_points_at_the_ws_client() {
        let page = play_page(None, None);
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

    /// Askama strips one trailing newline from a template file, and the eight
    /// hand-built pages were themselves inconsistent about it (the dashboard
    /// ended `</html>\n`, the play and history pages ended `</html>`). No
    /// assertion can observe it, so the migration guards compare modulo the
    /// final newline and hold every other byte exactly.
    fn same_html(rendered: &str, legacy: &str) {
        assert_eq!(
            rendered.trim_end_matches('\n'),
            legacy.trim_end_matches('\n')
        );
    }

    /// Temporary migration guard: proves the Askama template renders the exact
    /// same bytes the hand-built `format!` version did, on both branches.
    /// Deleted together with `legacy_dashboard_page` once the page is settled.
    #[test]
    fn dashboard_template_matches_legacy() {
        let summary = active_summary(true).unwrap();
        same_html(
            &dashboard_page(Some(&summary)).unwrap(),
            &legacy_dashboard_page(Some(&summary)),
        );
        same_html(&dashboard_page(None).unwrap(), &legacy_dashboard_page(None));
    }

    /// Temporary migration guard; see [`dashboard_template_matches_legacy`].
    #[test]
    fn analysis_template_matches_legacy() {
        same_html(&analysis_page().unwrap(), &legacy_analysis_page());
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
        let page = tournaments_page(&empty, 1, 1);
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
        let page = tournaments_page(&sessions, 1, 1);
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
        assert!(page.contains("canvas[data-points]"));
    }

    #[test]
    fn tournaments_page_escapes_database_strings() {
        let sessions = vec![(
            summary(1, r#"<script>"evil"</script>"#, "end", 1, 1, 0.0),
            vec![(1, 0.0)],
        )];
        let page = tournaments_page(&sessions, 1, 1);
        assert!(!page.contains(r#"<script>"evil""#));
        assert!(page.contains("&lt;script&gt;"));
    }

    #[test]
    fn tournaments_page_renders_pagination_controls() {
        let empty: Vec<(SessionSummary, Vec<ChartPoint>)> = Vec::new();

        let first = tournaments_page(&empty, 1, 1);
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

        let middle = tournaments_page(&empty, 2, 3);
        assert!(middle.contains("Page 2 of 3"));
        assert!(
            middle.contains(r#"href="/tournaments?page=1"#),
            "the newer link points at the previous page: {middle}"
        );
        assert!(
            middle.contains(r#"href="/tournaments?page=3"#),
            "the older link points at the next page: {middle}"
        );

        let last = tournaments_page(&empty, 3, 3);
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
        let page = tournament_detail_page(&detail(7, Some("WIN"), Some(1500)));
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
        let loss = tournament_detail_page(&detail(9, Some("LOSS"), Some(0)));
        assert!(loss.contains(r#"class="pt-result-badge loss">LOSS</span>"#));

        let unknown = tournament_detail_page(&detail(9, None, None));
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
        let fragment = table_fragment(&state, 1, 0, &[], &[]);
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

        let fragment = table_fragment(&state, 3, 0, &["You check".to_string()], &[]);
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
        let fragment = table_fragment(&state, 1, 0, &[], &[]);
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
        let fragment = table_fragment(&state, 1, 0, &[], &[]);
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

        let fragment = table_fragment(&state, 1, 0, &[], &[]);
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

        let fragment = table_fragment(&state, 1, 0, &[], &[]);
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
        let waiting = table_fragment(&state, 1, 0, &[], &[]);
        assert!(waiting.contains("Waiting for"));

        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        let finished = table_fragment(&state, 1, 0, &[], &[]);
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
        let waiting = table_fragment(&state, 1, 0, &[], &[]);
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
        let hero_turn = table_fragment(&hero_state, 1, 2, &[], &[]);
        assert!(
            hero_turn.contains(r#"data-decision="h1-a2-preflop""#),
            "the decision token names hand, action count, and street: {hero_turn}",
        );

        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        let finished = table_fragment(&state, 1, 2, &[], &[]);
        assert!(
            !finished.contains("data-decision"),
            "no decision token once the hand is over: {finished}"
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

        let fragment = table_fragment(&state, 2, 0, &[], &[]);
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
        let fragment = table_fragment(&state, 4, 0, &[], &[]);
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

        let fragment = table_fragment(&state, 1, 0, &log, &[]);
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
        let fragment = table_fragment(&state, 5, 0, &[], &[]);
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
        let raised = table_fragment(&state, 5, 0, &[], &[]);
        assert!(
            raised.contains(r#"class="pt-bet">60</div>"#),
            "the raise amount shows in front of the raiser: {raised}"
        );

        // Close the betting round: street bets are swept into the pot pill.
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.advance_street(&mut deck).unwrap();
        let settled = table_fragment(&state, 5, 0, &[], &[]);
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

        let fragment = table_fragment(&state, 1, 0, &[], &[]);
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
        let fragment = table_fragment(&state, 1, 0, &[], &[]);
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
        let page = history_page(&hh_stats(), &hh_listing(), None);
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
        let page = history_page(&hh_stats(), &[], None);
        assert!(page.contains("No imported hand histories yet"));
        assert!(!page.contains("pt-hh-table"));
    }

    #[test]
    fn history_page_escapes_stored_strings() {
        let mut stats = hh_stats();
        stats.net_chips = -15;
        let mut listing = hh_listing();
        listing[0].tournament.name = r#"<script>"evil"</script>"#.to_string();
        let page = history_page(&stats, &listing, None);
        assert!(!page.contains(r#"<script>"evil""#));
        assert!(page.contains("&lt;script&gt;"));
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
        let page = history_scan_result_page(&hh_outcome());
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
        let page = history_scan_result_page(&outcome);
        assert!(!page.contains(r#"<script>"bad""#));

        let mut clean = hh_outcome();
        clean.failures = Vec::new();
        let page = history_scan_result_page(&clean);
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
        let page = history_tournament_detail_page(&hh_detail());
        assert!(page.contains("<title>Poker Trainer — Spin&amp;Gold #7</title>"));
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
        let page = history_tournament_detail_page(&detail);
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
        let page = play_page(Some(0.71), Some(0.62));
        assert!(page.contains("pt-skill-chip"), "{page}");
        assert!(
            page.contains("You <b>0.71</b> · Bots <b>0.62</b>"),
            "{page}"
        );

        let missing = play_page(None, Some(0.62));
        assert!(
            missing.contains("You <b>—</b> · Bots <b>0.62</b>"),
            "{missing}"
        );

        let none = play_page(None, None);
        assert!(!none.contains("pt-skill-chip"), "no store, no chip: {none}");
    }

    #[test]
    fn history_page_offers_the_analyzer_and_the_current_template() {
        let page = history_page(&hh_stats(), &hh_listing(), None);
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
        let page = history_page(&hh_stats(), &[], Some(&template));
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
        assert!(page.contains("/history/analyze-status"));
        assert!(page.contains("id=\"analysis-status\""), "{page}");
    }

    #[test]
    fn analysis_status_html_covers_every_job_state() {
        use crate::opponent_analysis::{FieldReport, JobState};

        let idle = analysis_status_html(&JobState::Idle);
        assert!(idle.contains("Analyze imported opponents"), "{idle}");

        let running = analysis_status_html(&JobState::Running {
            hands_done: 30,
            hands_total: 100,
        });
        assert!(running.contains("hand 30 of 100"), "{running}");

        let done = analysis_status_html(&JobState::Done(report_fixture()));
        assert!(done.contains("Hands in window</span><b>100</b>"), "{done}");
        assert!(done.contains("Field skill</span><b>0.62</b>"), "{done}");
        assert!(done.contains("14c11a2a"), "{done}");
        assert!(done.contains("0.300"), "{done}");
        assert!(done.contains(r#"action="/history/save-template""#));
        assert!(done.contains("engine call amount 40 differs"), "{done}");

        // A report without graded decisions never offers a save action.
        let empty = analysis_status_html(&JobState::Done(FieldReport::empty()));
        assert!(!empty.contains("save-template"), "{empty}");
    }
}
