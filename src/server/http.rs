use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use sqlx::PgPool;
use tower_http::services::ServeDir;

use crate::analytics;
use crate::blunder::BlunderConfig;
use crate::decision::SurvivalConfig;
use crate::error::Result;
use crate::mcts::MctsConfig;
use crate::server::{views, ws};

/// Shared server state injected into handlers: static assets, the solver
/// configuration used for every table session, the optional analytics store
/// backing decision persistence and the tournaments page, how many
/// chart ticks pass between decimated snapshot refreshes, and how long the
/// winner stays on screen before the next hand is dealt.
#[derive(Clone, Debug)]
pub struct AppState {
    pub assets: ServeDir,
    pub mcts: MctsConfig,
    pub survival: SurvivalConfig,
    pub blunder: BlunderConfig,
    pub pool: Option<PgPool>,
    pub snapshot_interval: usize,
    pub result_pause_ms: u64,
}

/// Serves the repository `assets/` directory, anchored at the crate manifest
/// so it works regardless of the process working directory.
pub fn default_assets() -> ServeDir {
    ServeDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/assets"))
}

/// Assembles the application router: the shell page, a health probe, static
/// assets, and the WebSocket upgrade endpoint.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/tournaments", get(tournaments))
        .route("/tournaments/{id}", get(tournament_detail))
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

async fn index() -> Html<String> {
    Html(views::index_page())
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
        Ok(pageview) => Html(views::tournaments_page(
            &pageview.sessions,
            pageview.page,
            pageview.pages,
        ))
        .into_response(),
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
        Ok(Some(detail)) => Html(views::tournament_detail_page(&detail)).into_response(),
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
    async fn index_serves_the_shell_page() {
        let (status, body) = get("/").await;
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
    async fn tournaments_requires_an_analytics_store() {
        let (status, body) = get("/tournaments").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("analytics store is unavailable"));
    }

    #[tokio::test]
    async fn tournaments_page_lists_finished_sessions_with_charts() {
        use crate::game::Street;

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        dotenvy::dotenv().ok();
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => panic!(
                "DATABASE_URL is required for database integration tests; start PostgreSQL via pg.ps1"
            ),
        };
        let pool = crate::db::connect(&database_url).await.unwrap();
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
        dotenvy::dotenv().ok();
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => panic!(
                "DATABASE_URL is required for database integration tests; start PostgreSQL via pg.ps1"
            ),
        };
        let pool = crate::db::connect(&database_url).await.unwrap();
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
        dotenvy::dotenv().ok();
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => panic!(
                "DATABASE_URL is required for database integration tests; start PostgreSQL via pg.ps1"
            ),
        };
        let pool = crate::db::connect(&database_url).await.unwrap();
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
}
