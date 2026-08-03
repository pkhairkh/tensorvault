//! # Postgres wire protocol server.
//!
//! Minimal but spec-compliant PostgreSQL v3 frontend/backend protocol.
//! Supports: startup (SSL refused with N, trust auth), simple query (Q),
//! extended query (P/B/D/E/S/C/X/H), and ErrorResponse for unsupported msgs.

pub mod pgwire;
pub mod session;

pub use pgwire::PgConn;
pub use session::Session;

use crate::engine::QueryEngine;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Bind address (use port 0 for ephemeral).
    pub addr: SocketAddr,
    /// Server name reported in ParameterStatus.
    pub server_name: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { addr: "127.0.0.1:0".parse().unwrap(), server_name: "turboGP".into() }
    }
}

/// A running turboGP server.
pub struct Server {
    /// Actual bound address.
    pub local_addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl Server {
    /// Bind and spawn the accept loop. Must be called inside a Tokio runtime.
    pub async fn bind(
        engine: Arc<RwLock<QueryEngine>>,
        config: ServerConfig,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(config.addr)
            .await
            .map_err(|e| std::io::Error::new(e.kind(), format!("bind {}: {e}", config.addr)))?;
        let local_addr = listener.local_addr()?;
        let server_name = config.server_name.clone();

        let handle = tokio::spawn(async move {
            log::debug!("turboGP listening on {local_addr}");
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let engine = Arc::clone(&engine);
                        let name = server_name.clone();
                        tokio::spawn(async move {
                            if let Err(e) = PgConn::handle(stream, peer, engine, name).await {
                                log::debug!("conn {peer}: {e}");
                            }
                        });
                    }
                    Err(e) => { log::error!("accept: {e}"); break; }
                }
            }
        });

        Ok(Server { local_addr, handle })
    }

    /// Wait for the server task to finish (normally never).
    pub async fn join(self) -> Result<(), tokio::task::JoinError> { self.handle.await }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_default() {
        let c = ServerConfig::default();
        assert_eq!(c.addr.port(), 0);
        assert_eq!(c.server_name, "turboGP");
    }
    #[tokio::test]
    async fn bind_returns_local_addr() {
        let engine = Arc::new(RwLock::new(QueryEngine::new()));
        let s = Server::bind(engine, ServerConfig::default()).await.unwrap();
        assert_ne!(s.local_addr.port(), 0);
    }
}
