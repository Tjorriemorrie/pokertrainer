use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use serde_json::json;
use tower_http::services::ServeDir;

use crate::decision::SurvivalConfig;
use crate::error::Result;
use crate::mcts::MctsConfig;
use crate::server::{views, ws};

/// Shared server state injected into handlers: static assets and the solver
/// configuration used for every table session.
#[derive(Clone, Debug)]
pub struct AppState {
    pub assets: ServeDir,
    pub mcts: MctsConfig,
    pub survival: SurvivalConfig,
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
        })
    }

    async fn get(path: &str) -> (StatusCode, String) {
        let response = router(test_state())
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
