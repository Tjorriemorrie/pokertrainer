//! The local HTTP + WebSocket server serving the table UI (S7).
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
    max_depth: 3,
};

/// Everything the server needs to run: the bind address and the solver
/// configuration applied to every new table session.
#[derive(Clone, Copy, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub mcts: MctsConfig,
    pub survival: SurvivalConfig,
    pub blunder: BlunderConfig,
}

impl ServerConfig {
    /// The default local configuration: loopback-only, live solver budget.
    pub fn live() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8744)),
            mcts: LIVE_MCTS,
            survival: SurvivalConfig::default(),
            blunder: BlunderConfig::default(),
        }
    }
}

/// Boots the server on the configured address and serves until the process
/// ends.
pub async fn serve(config: ServerConfig) -> Result<()> {
    let state = Arc::new(AppState {
        assets: http::default_assets(),
        mcts: config.mcts,
        survival: config.survival,
        blunder: config.blunder,
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
    }

    #[tokio::test]
    async fn serve_starts_and_accepts_connections() {
        let bind = ephemeral_addr();
        let task = tokio::spawn(serve(ServerConfig {
            bind,
            mcts: MctsConfig::test(),
            survival: SurvivalConfig::default(),
            blunder: crate::blunder::BlunderConfig::default(),
        }));

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
