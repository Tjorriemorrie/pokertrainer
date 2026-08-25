use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde_json::json;
use sqlx::PgPool;
use tower_http::services::ServeDir;

use crate::analytics;
use crate::blunder::BlunderConfig;
use crate::decision::SurvivalConfig;
use crate::error::Result;
use crate::hh;
use crate::mcts::MctsConfig;
use crate::opponent_analysis::{self, JobState, job_guard};
use crate::server::{views, ws};

/// Shared server state injected into handlers: static assets, the solver
/// configuration used for every table session, the optional analytics store
/// backing decision persistence and the tournaments page, how many
/// chart ticks pass between decimated snapshot refreshes, how long the
/// winner stays on screen before the next hand is dealt, where the
/// GGPoker hand-history zips live, and the shared background-analysis job.
#[derive(Clone, Debug)]
pub struct AppState {
    pub assets: ServeDir,
    pub mcts: MctsConfig,
    pub survival: SurvivalConfig,
    pub blunder: BlunderConfig,
    pub pool: Option<PgPool>,
    pub snapshot_interval: usize,
    pub result_pause_ms: u64,
    pub history_dir: PathBuf,
    pub analysis: Arc<Mutex<JobState>>,
}

/// Serves the repository `assets/` directory, anchored at the crate manifest
/// so it works regardless of the process working directory.
pub fn default_assets() -> ServeDir {
    ServeDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/assets"))
}

/// Assembles the application router: the dashboard, the playing table, a
/// health probe, static assets, and the WebSocket upgrade endpoint.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/play", get(play))
        .route("/health", get(health))
        .route("/tournaments", get(tournaments))
        .route("/tournaments/{id}", get(tournament_detail))
        .route("/history", get(history))
        .route("/history/scan", post(history_scan))
        .route("/history/tournaments/{id}", get(history_tournament_detail))
        .route("/history/analyze", get(history_analyze_page))
        .route("/history/analyze-status", get(history_analyze_status))
        .route(
            "/history/analyze-opponents",
            post(history_analyze_opponents),
        )
        .route("/history/save-template", post(history_save_template))
        .route("/history/clear-template", post(history_clear_template))
        .route("/ws", get(ws::handler))
        .nest_service("/assets", state.assets.clone())
        .with_state(state)
}

/// Binds and serves until the process is stopped.
pub async fn serve(bind: std::net::SocketAddr, state: Arc<AppState>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    ServeListener::new(listener, state).await_forever().await
}

/// A listener-bound server, split out so tests can bind port 0 first and
/// discover the assigned address.
pub struct ServeListener {
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
}

impl ServeListener {
    pub fn new(listener: tokio::net::TcpListener, state: Arc<AppState>) -> Self {
        Self { listener, state }
    }

    pub async fn await_forever(self) -> Result<()> {
        tracing::info!(addr = %self.local_addr(), "pokertrainer table server listening");
        axum::serve(self.listener, router(self.state)).await?;
        Ok(())
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.listener.local_addr().unwrap_or_else(|_| {
            std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0)
        })
    }
}

/// Turns a rendered page into an HTML response, mirroring the JSON error
/// contract every other failing handler already uses. A template only fails to
/// render if a `Display` impl fails, so this is a "cannot happen" path that must
/// still not panic (see AGENTS.md).
fn html(rendered: Result<String>) -> Response {
    match rendered {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::warn!(%error, "page failed to render");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

/// The dashboard landing page: the resume card for the active tournament (if
/// any) or a start button. Without a database the dashboard has nothing to
/// read, so it always offers a fresh start.
async fn dashboard(State(app): State<Arc<AppState>>) -> Response {
    let active = match app.pool.as_ref() {
        Some(pool) => match crate::live::load_dashboard(pool).await {
            Ok(Some(active)) => Some(active.summary),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "dashboard could not read the active tournament");
                None
            }
        },
        None => None,
    };
    html(views::dashboard_page(active.as_ref()))
}

async fn play(State(app): State<Arc<AppState>>) -> Response {
    let (you, bots) = match app.pool.as_ref() {
        Some(pool) => {
            let you = match opponent_analysis::hero_skill(pool).await {
                Ok(skill) => skill,
                Err(error) => {
                    tracing::warn!(%error, "hero skill unavailable");
                    None
                }
            };
            let bots = match opponent_analysis::load_template(pool).await {
                Ok(Some(template)) => Some(template.skill),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(%error, "bot template unavailable");
                    None
                }
            };
            (you, bots)
        }
        None => (None, None),
    };
    Html(views::play_page(you, bots)).into_response()
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(json!({ "status": "ok" })))
}

/// How many finished tournaments render per page of the history listing.
pub const TOURNAMENTS_PAGE_SIZE: i64 = 25;

/// The parsed `?page=` query parameter of `/tournaments`.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
pub struct TournamentsParams {
    pub page: Option<u32>,
}

/// One rendered page of the tournament history.
#[derive(Clone, Debug)]
pub struct TournamentsPage {
    pub sessions: Vec<(analytics::SessionSummary, Vec<analytics::ChartPoint>)>,
    pub page: u32,
    pub pages: u32,
}

/// The finished-tournament history page: a paginated listing (25 per page,
/// newest first) of one decimated EV chart per finished session. Without a
/// database this endpoint cannot render anything.
async fn tournaments(
    State(app): State<Arc<AppState>>,
    Query(params): Query<TournamentsParams>,
) -> Response {
    let Some(pool) = app.pool.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "error": "analytics store is unavailable" })),
        )
            .into_response();
    };
    let page = params.page.unwrap_or(1).max(1);
    match render_tournaments(&pool, page).await {
        Ok(pageview) => html(views::tournaments_page(
            &pageview.sessions,
            pageview.page,
            pageview.pages,
        )),
        Err(error) => {
            tracing::warn!(%error, page, "tournaments page failed to render");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

/// Loads one page of finished sessions (newest first) plus their decimated
/// chart datasets.
pub async fn render_tournaments(pool: &PgPool, page: u32) -> Result<TournamentsPage> {
    let total = analytics::count_finished_sessions(pool).await?;
    let pages = (((total + TOURNAMENTS_PAGE_SIZE - 1) / TOURNAMENTS_PAGE_SIZE) as u32).max(1);
    let offset = (page - 1) as i64 * TOURNAMENTS_PAGE_SIZE;
    let summaries = analytics::list_finished_sessions(pool, TOURNAMENTS_PAGE_SIZE, offset).await?;
    let mut sessions = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let points = analytics::load_session(pool, summary.id, analytics::CHART_WINDOW).await?;
        sessions.push((
            summary,
            analytics::decimate(&points, analytics::DECIMATED_POINTS),
        ));
    }
    Ok(TournamentsPage {
        sessions,
        page,
        pages,
    })
}

/// The single-tournament detail page: one finished session's hand-level
/// aggregates, EV stats, and decimated chart.
async fn tournament_detail(State(app): State<Arc<AppState>>, Path(id): Path<i32>) -> Response {
    let Some(pool) = app.pool.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "error": "analytics store is unavailable" })),
        )
            .into_response();
    };
    match analytics::load_tournament_detail(&pool, id).await {
        Ok(Some(detail)) => html(views::tournament_detail_page(&detail)),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "tournament not found" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, tournament_id = id, "tournament detail page failed to render");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

/// The GGPoker hand-history page: the scan trigger, lifetime aggregates, and
/// the imported-tournament listing (newest first), plus the opponent-skill
/// analyzer entry and the current bot template.
async fn history(State(app): State<Arc<AppState>>) -> Response {
    let Some(pool) = app.pool.clone() else {
        return unavailable("analytics store is unavailable");
    };
    let page = match history_page_data(&pool).await {
        Ok((stats, tournaments, template)) => {
            views::history_page(&stats, &tournaments, template.as_ref())
        }
        Err(error) => {
            tracing::warn!(%error, "history page failed to render");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    Html(page).into_response()
}

/// Loads the data driving the history page: lifetime stats, the listing, and
/// the stored bot template.
async fn history_page_data(
    pool: &PgPool,
) -> Result<(
    hh::OverallStats,
    Vec<hh::TournamentListing>,
    Option<opponent_analysis::DrillTemplate>,
)> {
    let stats = hh::overall_stats(pool).await?;
    let tournaments = hh::list_tournaments(pool).await?;
    let template = opponent_analysis::load_template(pool).await?;
    Ok((stats, tournaments, template))
}

/// Scans the configured history directory, imports the found hands, and
/// renders the results page restricted to the newly imported hands.
async fn history_scan(State(app): State<Arc<AppState>>) -> Response {
    let Some(pool) = app.pool.clone() else {
        return unavailable("analytics store is unavailable");
    };
    let run = match hh::scan_directory(&app.history_dir) {
        Ok(run) => run,
        Err(error) => return scan_failure(error),
    };
    match hh::import_scan(&pool, &run).await {
        Ok(outcome) => Html(views::history_scan_result_page(&outcome)).into_response(),
        Err(error) => {
            tracing::warn!(%error, "hand history scan failed to import");
            scan_failure(error)
        }
    }
}

fn scan_failure(error: crate::error::Error) -> Response {
    tracing::warn!(%error, "hand history scan failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({ "error": error.to_string() })),
    )
        .into_response()
}

/// The opponent-analysis page: a polling shell whose status fragment is
/// refreshed from `/history/analyze-status`.
async fn history_analyze_page() -> Response {
    html(views::analysis_page())
}

/// One JSON status payload for the analysis page: the job state plus the
/// rendered HTML fragment to swap in.
async fn history_analyze_status(State(app): State<Arc<AppState>>) -> Response {
    let (state, html) = match job_guard(&app.analysis) {
        Ok(status) => (
            match &*status {
                JobState::Idle => "idle",
                JobState::Running { .. } => "running",
                JobState::Done(_) => "done",
            },
            views::analysis_status_html(&status),
        ),
        Err(error) => {
            tracing::warn!(%error, "analysis status lock poisoned");
            ("idle", views::analysis_status_html(&JobState::Idle))
        }
    };
    axum::Json(json!({ "state": state, "html": html })).into_response()
}

/// Starts the background opponent analyzer when no job is running, then
/// redirects to the analysis page. Only one analysis runs at a time.
async fn history_analyze_opponents(State(app): State<Arc<AppState>>) -> Response {
    let Some(pool) = app.pool.clone() else {
        return unavailable("analytics store is unavailable");
    };
    let start = match job_guard(&app.analysis) {
        Ok(mut status) => {
            let start = matches!(*status, JobState::Idle);
            if start {
                *status = JobState::Running {
                    hands_done: 0,
                    hands_total: 0,
                };
            }
            start
        }
        Err(error) => {
            tracing::warn!(%error, "analysis job lock poisoned");
            false
        }
    };
    if start {
        tokio::spawn({
            let analysis = app.analysis.clone();
            async move {
                if let Err(error) =
                    opponent_analysis::run_job(pool, analysis.clone(), MctsConfig::analysis()).await
                {
                    tracing::warn!(%error, "opponent analysis job failed");
                    if let Ok(mut status) = analysis.lock() {
                        *status = JobState::Idle;
                    }
                }
            }
        });
        tracing::info!("opponent analysis job started");
    }
    Redirect::to("/history/analyze").into_response()
}

async fn history_save_template(State(app): State<Arc<AppState>>) -> Response {
    let Some(pool) = app.pool.clone() else {
        return unavailable("analytics store is unavailable");
    };
    let report = match job_guard(&app.analysis) {
        Ok(status) => match &*status {
            JobState::Done(report) => Some(report.clone()),
            _ => None,
        },
        Err(error) => {
            tracing::warn!(%error, "analysis job lock poisoned");
            None
        }
    };
    let Some(report) = report else {
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": "no finished analysis to save — run the analyzer first" })),
        )
            .into_response();
    };
    let label = format!("Imported field ({} decisions)", report.decisions);
    match opponent_analysis::save_template(
        &pool,
        &label,
        report.skill,
        report.avg_ev_loss_bb,
        report.decisions.min(i64::from(i32::MAX)) as i32,
    )
    .await
    {
        Ok(()) => Redirect::to("/history").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn history_clear_template(State(app): State<Arc<AppState>>) -> Response {
    let Some(pool) = app.pool.clone() else {
        return unavailable("analytics store is unavailable");
    };
    match opponent_analysis::clear_template(&pool).await {
        Ok(()) => Redirect::to("/history").into_response(),
        Err(error) => {
            tracing::warn!(%error, "template clear failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

/// One imported tournament's detail: stored summary, aggregates, and hands.
async fn history_tournament_detail(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(pool) = app.pool.clone() else {
        return unavailable("analytics store is unavailable");
    };
    match hh::load_tournament(&pool, &id).await {
        Ok(Some(detail)) => Html(views::history_tournament_detail_page(&detail)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "tournament not found" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, tournament_id = %id, "history tournament detail failed to render");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

fn unavailable(message: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({ "error": message })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: None,
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        })
    }

    async fn get(path: &str) -> (StatusCode, String) {
        get_with(test_state(), path).await
    }

    async fn get_with(state: Arc<AppState>, path: &str) -> (StatusCode, String) {
        let response = router(state)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn dashboard_serves_a_start_when_nothing_is_active() {
        let (status, body) = get("/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Poker Trainer</title>"));
        assert!(
            body.contains(r#"href="/play">Start tournament</a>"#),
            "the dashboard offers a start without a database: {body}"
        );
        assert!(!body.contains(r#"id="table""#), "the table lives on /play");
    }

    #[tokio::test]
    async fn play_serves_the_table_shell() {
        let (status, body) = get("/play").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Poker Trainer</title>"));
        assert!(body.contains(r#"id="table""#));
        assert!(body.contains("/assets/app.js"));
    }

    #[tokio::test]
    async fn health_reports_ok() {
        let (status, body) = get("/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn unknown_routes_return_404() {
        let (status, _) = get("/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dashboard_shows_the_active_tournament_resume_card() {
        use crate::snapshot::{OpponentCountersSnapshot, StateSnapshot, TournamentSnapshot};

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;

        // Free any pre-existing active row so the dashboard state is known.
        sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
            .execute(&pool)
            .await
            .unwrap();

        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });

        let (status, body) = get_with(state.clone(), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Start tournament"),
            "a dashboard without an active row offers a start: {body}"
        );

        let session_id = match crate::live::claim_or_resume(&pool).await.unwrap() {
            crate::live::ClaimOutcome::Fresh(id) => id,
            other => panic!("expected a fresh claim, got {other:?}"),
        };
        crate::live::save_snapshot(
            &pool,
            session_id,
            &TournamentSnapshot {
                state: StateSnapshot {
                    stacks: [460, 480, 460],
                    button: 0,
                    blind_small: 10,
                    blind_big: 20,
                    street: 1,
                    board: vec!["2c".into(), "7h".into(), "Kd".into()],
                    hole_cards: vec![
                        ["As".into(), "Kd".into()],
                        ["2c".into(), "2h".into()],
                        ["7s".into(), "8s".into()],
                    ],
                    revealed: [true, false, false],
                    street_contrib: [0, 0, 0],
                    total_contrib: [10, 10, 10],
                    current_bet: 0,
                    min_raise: 20,
                    last_full_raise: None,
                    acted: [false, false, false],
                    folded: [false, false, false],
                    all_in: [false, false, false],
                    eliminated: [false, false, false],
                    to_act: 0,
                    hand_over: false,
                    hand_result: None,
                },
                deck: Vec::new(),
                hand_no: 6,
                action_no: 13,
                log: Vec::new(),
                template_skill: None,
                opponents: OpponentCountersSnapshot {
                    hands: [6, 6],
                    vpip: [1, 2],
                    pfr: [0, 1],
                    faced_bet: [2, 1],
                    folded_to_bet: [1, 0],
                    postflop_bets: [1, 0],
                    postflop_calls: [0, 2],
                    vpip_seen: [false, false],
                    pfr_seen: [false, false],
                },
            },
        )
        .await
        .unwrap();
        crate::live::mark_disconnected(&pool).await.unwrap();

        let (status, body) = get_with(state, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"href="/play">Resume tournament</a>"#),
            "an active tournament renders the resume card: {body}"
        );
        assert!(body.contains("Hand</span><b>#6</b>"));
        assert!(body.contains("Your stack</span><b>460</b>"));

        crate::live::clear_active(&pool).await.unwrap();
        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tournaments_requires_an_analytics_store() {
        let (status, body) = get("/tournaments").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("analytics store is unavailable"));
    }

    #[tokio::test]
    async fn tournaments_page_lists_finished_sessions_with_charts() {
        use crate::game::Street;

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
        let session_id = analytics::start_session(&pool).await.unwrap();
        analytics::persist_records(
            &pool,
            session_id,
            &[analytics::PendingDecision {
                hand_no: 3,
                street: Street::Turn,
                played: "Call".into(),
                optimal: "Fold".into(),
                ev_loss: 12.5,
            }],
        )
        .await
        .unwrap();
        analytics::finish_session(&pool, session_id).await.unwrap();

        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });
        let (status, body) = get_with(state, "/tournaments").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Poker Trainer — Tournaments</title>"));
        assert!(body.contains(&format!("data-tournament-id=\"{session_id}\"")));
        assert!(body.contains("3 hands"));
        assert!(body.contains("12.5"));

        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tournaments_page_paginates_newest_first() {
        use crate::game::Street;

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
        // Real (or older) finished sessions may already exist; the listing
        // must stay newest-first, so the page math is derived relative to
        // them instead of assuming an empty database.
        let prior = analytics::count_finished_sessions(&pool).await.unwrap();

        // One page plus a spill-over, finished in ascending-id order.
        let mut ids = Vec::new();
        for hand in 1..=TOURNAMENTS_PAGE_SIZE + 4 {
            let id = analytics::start_session(&pool).await.unwrap();
            analytics::persist_records(
                &pool,
                id,
                &[analytics::PendingDecision {
                    hand_no: hand as u64,
                    street: Street::Preflop,
                    played: "Call".into(),
                    optimal: "Fold".into(),
                    ev_loss: 1.0,
                }],
            )
            .await
            .unwrap();
            analytics::finish_session(&pool, id).await.unwrap();
            ids.push(id);
        }

        let total = prior + ids.len() as i64;
        let pages = (((total + TOURNAMENTS_PAGE_SIZE - 1) / TOURNAMENTS_PAGE_SIZE) as u32).max(1);

        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });
        let newest_id = *ids.last().unwrap();
        let oldest_id = ids[0];
        let second_page_first = ids[ids.len() - TOURNAMENTS_PAGE_SIZE as usize - 1];

        let (status, first) = get_with(state.clone(), "/tournaments").await;
        assert_eq!(status, StatusCode::OK);
        let newest_marker = format!("data-tournament-id=\"{newest_id}\"");
        let oldest_marker = format!("data-tournament-id=\"{oldest_id}\"");
        assert!(
            first.contains(&newest_marker),
            "page 1 leads with the latest tournament: missing {newest_marker}"
        );
        let newest_at = first.find(&newest_marker);
        let second_at = first.find(&format!("data-tournament-id=\"{}\"", ids[ids.len() - 2]));
        assert!(
            newest_at.is_some() && second_at.is_some() && newest_at.unwrap() < second_at.unwrap(),
            "the listing runs latest-first"
        );
        assert!(!first.contains(&oldest_marker));
        assert!(first.contains(&format!("Page 1 of {pages}")));

        let (status, second) = get_with(state.clone(), "/tournaments?page=2").await;
        assert_eq!(status, StatusCode::OK);
        let spill_marker = format!("data-tournament-id=\"{second_page_first}\"");
        assert!(
            second.contains(&spill_marker),
            "page 2 starts where page 1 left off: missing {spill_marker}"
        );
        assert!(!second.contains(&format!("data-tournament-id=\"{newest_id}\"")));
        assert!(second.contains(&format!("Page 2 of {pages}")));

        let (status, beyond) = get_with(state.clone(), "/tournaments?page=999").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !beyond.contains("data-tournament-id"),
            "pages past the last one are empty: {beyond}"
        );
        assert!(beyond.contains(&format!("Page 999 of {pages}")), "{beyond}");

        sqlx::query("DELETE FROM hero_sessions WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tournament_detail_page_renders_one_finished_tournament() {
        use crate::game::Street;

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
        let session_id = analytics::start_session(&pool).await.unwrap();
        analytics::persist_records(
            &pool,
            session_id,
            &[analytics::PendingDecision {
                hand_no: 1,
                street: Street::Preflop,
                played: "Call".into(),
                optimal: "Fold".into(),
                ev_loss: 4.0,
            }],
        )
        .await
        .unwrap();
        analytics::persist_hand_results(
            &pool,
            session_id,
            &[analytics::PendingHandResult {
                hand_no: 1,
                hero_won: true,
                hero_all_in: false,
                hero_busted: false,
                winner_seat: 0,
            }],
        )
        .await
        .unwrap();
        analytics::finalize_session(&pool, session_id, "WIN", 1500)
            .await
            .unwrap();

        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });
        let (status, body) = get_with(state, &format!("/tournaments/{session_id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&format!(
            "<title>Poker Trainer — Tournament #{session_id}</title>"
        )));
        assert!(body.contains(r#"class="pt-result-badge win">WIN</span>"#));
        assert!(body.contains("<span>Hands won</span><b>1</b>"));
        assert!(body.contains("Final stack: 1500 chips"));

        let (missing, _) = get_with(
            Arc::new(AppState {
                assets: default_assets(),
                mcts: MctsConfig::test(),
                survival: SurvivalConfig::default(),
                blunder: BlunderConfig::default(),
                pool: Some(pool.clone()),
                snapshot_interval: 100,
                result_pause_ms: 0,
                history_dir: crate::hh::default_history_dir(),
                analysis: Arc::new(Mutex::new(JobState::Idle)),
            }),
            "/tournaments/999999999",
        )
        .await;
        assert_eq!(missing, StatusCode::NOT_FOUND);

        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    /// A template only fails to render if a `Display` impl fails, so no route
    /// can reach this branch in practice — but it must degrade to the same JSON
    /// error shape as every other failing handler rather than panic.
    #[test]
    fn a_failed_render_becomes_a_json_500() {
        let response = html(Err(crate::error::Error::Analytics("boom".to_string())));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn static_assets_are_served_from_the_repository_dir() {
        let (status, body) = get("/assets/style.css").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("pt-topwrap"));

        let (status, _) = get("/assets/missing.js").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_binds_and_answers_http() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = probe.local_addr().unwrap();
        drop(probe);

        let task = tokio::spawn(serve(bind, test_state()));
        let mut answered = false;
        for _ in 0..100 {
            if let Ok(mut stream) = tokio::net::TcpStream::connect(bind).await {
                stream
                    .write_all(b"GET /health HTTP/1.1\r\nHost: local\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                let mut buf = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                let response = String::from_utf8_lossy(&buf);
                assert!(
                    response.contains("200 OK"),
                    "unexpected response: {response}"
                );
                assert!(response.contains(r#"{"status":"ok"}"#));
                answered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(answered, "server did not answer HTTP on {bind}");
        task.abort();
    }

    #[tokio::test]
    async fn websocket_endpoint_requires_an_upgrade() {
        let response = router(test_state())
            .oneshot(Request::builder().uri("/ws").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::OK);
    }

    // ------------------------------------------------------- hand history

    async fn post_with(state: Arc<AppState>, path: &str) -> (StatusCode, String) {
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn history_requires_an_analytics_store() {
        let (status, body) = get("/history").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("analytics store is unavailable"));

        let (status, body) = post_with(test_state(), "/history/scan").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("analytics store is unavailable"));

        let (status, _) = get("/history/tournaments/1").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn history_scan_imports_zips_and_renders_new_hand_stats() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tournament_id = format!("T{unique}");
        let hand_id = format!("SG{unique}");
        let dir = std::env::temp_dir().join(format!("pokertrainer_http_scan_{unique}"));
        std::fs::create_dir_all(&dir).unwrap();

        let hand_text = format!(
            "Poker Hand #{hand_id}: Tournament #{tournament_id}, Spin&Gold #7 Hold'em No Limit - Level1(10/20) - 2026/08/21 15:07:44
Table '39856' 3-max Seat #2 is the button
Seat 2: Hero (540 in chips)
Seat 3: 14c11a2a (360 in chips)
Hero: posts small blind 10
14c11a2a: posts big blind 20
*** HOLE CARDS ***
Dealt to Hero [As Kh]
Hero: raises 20 to 40
14c11a2a: calls 20
*** FLOP *** [2c 7h 9d]
Hero: bets 30
14c11a2a: folds
Uncalled bet (30) returned to Hero
*** SHOWDOWN ***
Hero collected 80 from pot
*** SUMMARY ***
Total pot 80 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Board [2c 7h 9d]
Seat 2: Hero (small blind) collected (80)
"
        );
        let summary_text = format!(
            "Tournament #{tournament_id}, Spin&Gold #7, Hold'em No Limit
Buy-in: $0.25
3 Players
Total Prize Pool: $0.75
Tournament started 2026/08/21 15:03:37 
1st : Hero, $0.75
You finished in 1st place.
"
        );
        let zip_path = dir.join("export.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("hands.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(hand_text.as_bytes()).unwrap();
        writer
            .start_file("summary.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(summary_text.as_bytes()).unwrap();
        writer.finish().unwrap();

        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: dir.clone(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });

        let (status, body) = get_with(state.clone(), "/history").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Hand history"));
        assert!(body.contains(r#"action="/history/scan""#));

        let (status, body) = post_with(state.clone(), "/history/scan").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Poker Trainer — Scan results</title>"));
        assert!(body.contains("<span>New hands</span><b>1</b>"), "{body}");
        assert!(body.contains("<span>New tournaments</span><b>1</b>"));
        assert!(body.contains("<span>Won</span><b>1</b>"));
        assert!(body.contains("<span>Win ratio</span><b>100%</b>"), "{body}");

        // A re-scan is idempotent: nothing new, hand skipped.
        let (status, body) = post_with(state.clone(), "/history/scan").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<span>New hands</span><b>0</b>"), "{body}");
        assert!(body.contains("<span>Already imported</span><b>1</b>"));

        // The listing shows the tournament and the detail page the hand.
        let (status, body) = get_with(state.clone(), "/history").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&format!("href=\"/history/tournaments/{tournament_id}\"")));
        assert!(body.contains("$0.50"), "{body}");

        let (status, body) = get_with(
            state.clone(),
            &format!("/history/tournaments/{tournament_id}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("As Kh"));
        assert!(body.contains(r#"class="pt-result-badge win">WIN</span>"#));

        let (status, _) = get_with(state, "/history/tournaments/999999999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        sqlx::query("DELETE FROM gg_tournaments WHERE id = $1")
            .bind(&tournament_id)
            .execute(&pool)
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn history_scan_reports_bad_zips_without_failing_the_scan() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tournament_id = format!("T{unique}");
        let hand_id = format!("SG{unique}");
        let dir = std::env::temp_dir().join(format!("pokertrainer_http_scan_bad_{unique}"));
        std::fs::create_dir_all(&dir).unwrap();

        let zip_path = dir.join("mixed.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("junk.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"not poker").unwrap();
        writer
            .start_file("hands.txt", SimpleFileOptions::default())
            .unwrap();
        write!(
            writer,
            "Poker Hand #{hand_id}: Tournament #{tournament_id}, Spin&Gold #7 Hold'em No Limit - Level1(10/20) - 2026/08/21 15:07:44\n\
             Table '39856' 3-max Seat #2 is the button\n\
             Seat 2: Hero (540 in chips)\n\
             Seat 3: 14c11a2a (360 in chips)\n\
             Hero: posts small blind 10\n\
             14c11a2a: posts big blind 20\n\
             *** HOLE CARDS ***\n\
             Dealt to Hero [As Kh]\n\
             Hero: folds\n\
             *** SUMMARY ***\n\
             Seat 2: Hero folded\n"
        )
        .unwrap();
        writer.finish().unwrap();

        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: dir.clone(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });

        let (status, body) = post_with(state, "/history/scan").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<span>New hands</span><b>1</b>"), "{body}");
        assert!(
            body.contains("no recognizable PokerCraft content"),
            "the junk entry is listed as skipped: {body}"
        );

        sqlx::query("DELETE FROM gg_tournaments WHERE id = $1")
            .bind(&tournament_id)
            .execute(&pool)
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ------------------------------------------------------ opponent skill

    #[tokio::test]
    async fn analysis_endpoints_require_an_analytics_store() {
        let (status, body) = get("/history/analyze").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the shell page renders without a DB"
        );
        assert!(body.contains("analysis-status"));

        let (status, _) = post_with(test_state(), "/history/analyze-opponents").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let (status, _) = post_with(test_state(), "/history/save-template").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let (status, _) = post_with(test_state(), "/history/clear-template").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn analysis_status_reports_idle_and_the_page_polls() {
        let (status, body) = get("/history/analyze-status").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""state":"idle""#), "{body}");
        assert!(body.contains(r#""html""#), "{body}");

        let (status, body) = get("/history/analyze").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"src="/assets/analysis.js"#), "{body}");
    }

    #[tokio::test]
    async fn analyze_opponents_never_double_spawns_and_redirects() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Running {
                hands_done: 4,
                hands_total: 10,
            })),
        });
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/history/analyze-opponents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        // The pre-seeded progress is untouched: no second job replaced it.
        let status = state.analysis.lock().unwrap();
        assert!(matches!(
            *status,
            JobState::Running {
                hands_done: 4,
                hands_total: 10
            }
        ));
        let _ = response;
    }

    #[tokio::test]
    async fn save_and_clear_template_follow_the_finished_job() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
        crate::db::run_migrations(&pool).await.unwrap();
        crate::opponent_analysis::clear_template(&pool)
            .await
            .unwrap();

        // Without a finished job the save is refused outright.
        let idle_state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });
        let (status, _) = post_with(idle_state, "/history/save-template").await;
        assert_eq!(status, StatusCode::CONFLICT);

        let report = crate::opponent_analysis::FieldReport {
            hands_total: 40,
            hands_graded: 39,
            hands_failed: 1,
            decisions: 90,
            avg_ev_loss_bb: 0.35,
            skill: 0.77,
            players: Vec::new(),
            problems: Vec::new(),
        };
        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });
        {
            let mut status = state.analysis.lock().unwrap();
            *status = JobState::Done(report.clone());
        }

        let (status, _) = post_with(state.clone(), "/history/save-template").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let template = crate::opponent_analysis::load_template(&pool)
            .await
            .unwrap()
            .expect("the template row exists");
        assert_eq!(template.label, "Imported field (90 decisions)");
        assert!((template.skill - 0.77).abs() < 1e-9);
        assert_eq!(template.decisions, 90);

        let (status, _) = post_with(state, "/history/clear-template").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            crate::opponent_analysis::load_template(&pool)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn play_page_hides_the_skill_chip_without_a_store() {
        let (status, body) = get("/play").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("pt-skill-chip"), "{body}");
    }

    #[tokio::test]
    async fn play_page_compares_hero_and_field_skill_from_the_store() {
        use crate::game::Street;

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
        crate::db::run_migrations(&pool).await.unwrap();
        crate::opponent_analysis::clear_template(&pool)
            .await
            .unwrap();
        crate::opponent_analysis::save_template(
            &pool,
            "Imported field (4 decisions)",
            0.62,
            0.5,
            4,
        )
        .await
        .unwrap();

        let state = Arc::new(AppState {
            assets: default_assets(),
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            pool: Some(pool.clone()),
            snapshot_interval: 100,
            result_pause_ms: 0,
            history_dir: crate::hh::default_history_dir(),
            analysis: Arc::new(Mutex::new(JobState::Idle)),
        });
        let (status, body) = get_with(state.clone(), "/play").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("pt-skill-chip"), "{body}");
        assert!(body.contains("Bots <b>0.62</b>"), "{body}");

        let session = analytics::start_session(&pool).await.unwrap();
        analytics::persist_records(
            &pool,
            session,
            &[analytics::PendingDecision {
                hand_no: 1,
                street: Street::Preflop,
                played: "Call".into(),
                optimal: "Raise(60)".into(),
                ev_loss: 0.2,
            }],
        )
        .await
        .unwrap();
        let (status, body) = get_with(state.clone(), "/play").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("You <b>") && body.contains("· Bots <b>0.62</b>"),
            "the hero's lifetime skill lands next to the field's: {body}"
        );

        let (status, body) = get_with(state.clone(), "/history").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Bots trained on: Imported field (4 decisions)"),
            "{body}"
        );

        crate::opponent_analysis::clear_template(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();
    }
}
