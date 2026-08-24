//! The local HTTP + WebSocket server serving the table UI.
//!
//! Axum serves the rendered shell page and static assets over HTTP and swaps
//! HTML fragments into the client DOM over a WebSocket (see [`protocol`]).
//! Each connection owns a [`TableSession`] driven by the placeholder opponent
//! policy in [`session`].

pub mod http;
pub mod protocol;
pub mod session;
pub mod views;
pub mod ws;

pub use http::{AppState, ServeListener, router};
pub use session::{TableEvent, TableSession};

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::blunder::BlunderConfig;
use crate::decision::SurvivalConfig;
use crate::error::Result;
use crate::mcts::MctsConfig;

/// Solver budget used for live sessions: enough determinizations for stable
/// rankings while keeping an action feel responsive on a desktop machine.
pub const LIVE_MCTS: MctsConfig = MctsConfig {
    worlds: 16,
    iterations: 96,
    uct_c: 60.0,
    max_depth: 5,
    min_duration: Duration::from_secs(5),
    max_duration: Duration::from_secs(20),
};

/// Everything the server needs to run: the bind address, the solver
/// configuration applied to every new table session, how often the decimated
/// chart snapshot refreshes, and how long the winner stays on screen
/// before the next hand is dealt.
#[derive(Clone, Copy, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub mcts: MctsConfig,
    pub survival: SurvivalConfig,
    pub blunder: BlunderConfig,
    pub snapshot_interval: usize,
    pub result_pause_ms: u64,
}

impl ServerConfig {
    /// The default local configuration: loopback-only, live solver budget.
    pub fn live() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8744)),
            mcts: LIVE_MCTS,
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
            snapshot_interval: 100,
            result_pause_ms: 2000,
        }
    }
}

/// Boots the server on the configured address and serves until the process
/// ends. `pool` is the analytics store for decision persistence and the
/// tournaments page; [`None`] keeps the table playable without it.
pub async fn serve(config: ServerConfig, pool: Option<PgPool>) -> Result<()> {
    let state = Arc::new(AppState {
        assets: http::default_assets(),
        mcts: config.mcts,
        survival: config.survival,
        blunder: config.blunder,
        pool,
        snapshot_interval: config.snapshot_interval,
        result_pause_ms: config.result_pause_ms,
        history_dir: crate::hh::default_history_dir(),
        analysis: Arc::new(std::sync::Mutex::new(
            crate::opponent_analysis::JobState::Idle,
        )),
    });
    http::serve(config.bind, state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ephemeral_addr() -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    #[test]
    fn live_config_targets_the_local_port_and_valid_solver() {
        let config = ServerConfig::live();
        assert_eq!(config.bind.to_string(), "127.0.0.1:8744");
        config.mcts.validate().unwrap();
        config.blunder.validate().unwrap();
        assert!(config.snapshot_interval > 0);
        assert!(config.result_pause_ms > 0);
    }

    #[tokio::test]
    async fn serve_starts_and_accepts_connections() {
        let bind = ephemeral_addr();
        let task = tokio::spawn(serve(
            ServerConfig {
                bind,
                mcts: MctsConfig::test(),
                survival: SurvivalConfig::default(),
                blunder: crate::blunder::BlunderConfig::default(),
                snapshot_interval: 100,
                result_pause_ms: 0,
            },
            None,
        ));

        let mut connected = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(bind).await.is_ok() {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(connected, "server did not start listening on {bind}");
        task.abort();
    }
}
