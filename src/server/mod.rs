//! # Postgres wire protocol server.
//!
//! Minimal but spec-compliant PostgreSQL v3 frontend/backend protocol.
//! Supports: startup (SSL refused with N when TLS is unconfigured, trust
//! auth by default), simple query (Q), extended query (P/B/D/E/S/C/X/H),
//! ErrorResponse for unsupported msgs, and SCRAM-SHA-256 authentication
//! (Wave 65) when `ServerConfig::auth_required` is true.

pub mod auth;
pub mod pgwire;
pub mod session;

pub use auth::{PasswordManager, TlsConfig};
pub use pgwire::PgConn;
pub use session::Session;

use crate::engine::QueryEngine;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Server configuration.
#[derive(Clone)]
pub struct ServerConfig {
    /// Bind address (use port 0 for ephemeral).
    pub addr: SocketAddr,
    /// Server name reported in ParameterStatus.
    pub server_name: String,
    /// When true, the server requires SCRAM-SHA-256 authentication on
    /// every connection (Wave 65). When false (default), the server
    /// accepts any connection (backward compatible — the pre-Wave-65
    /// behavior).
    pub auth_required: bool,
    /// Optional TLS configuration (Wave 65). When `Some`, the server
    /// should respond 'S' to an SSLRequest and upgrade the connection.
    /// When `None`, the server responds 'N' (no SSL) and proceeds in
    /// plaintext (the default, backward-compatible behavior).
    pub tls: Option<TlsConfig>,
    /// Shared password manager. Required when `auth_required` is true;
    /// ignored otherwise. Mutated by `CREATE USER` / `DROP USER` SQL
    /// statements routed through the pgwire layer.
    pub passwords: Arc<RwLock<PasswordManager>>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("addr", &self.addr)
            .field("server_name", &self.server_name)
            .field("auth_required", &self.auth_required)
            .field("tls", &self.tls)
            .field("passwords", &format!("<{} users>", self.passwords.read().map(|m| m.len()).unwrap_or(0)))
            .finish()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:0".parse().unwrap(),
            server_name: "turboGP".into(),
            auth_required: false,
            tls: None,
            passwords: Arc::new(RwLock::new(PasswordManager::new())),
        }
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
        let auth_required = config.auth_required;
        let tls = config.tls.clone();
        let passwords = Arc::clone(&config.passwords);

        let handle = tokio::spawn(async move {
            log::debug!("turboGP listening on {local_addr} (auth_required={auth_required}, tls={})", tls.is_some());
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let engine = Arc::clone(&engine);
                        let name = server_name.clone();
                        let passwords = Arc::clone(&passwords);
                        let tls = tls.clone();
                        tokio::spawn(async move {
                            if let Err(e) = PgConn::handle(stream, peer, engine, name, auth_required, tls, passwords).await {
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
        assert!(!c.auth_required);
        assert!(c.tls.is_none());
    }
    #[tokio::test]
    async fn bind_returns_local_addr() {
        let engine = Arc::new(RwLock::new(QueryEngine::new()));
        let s = Server::bind(engine, ServerConfig::default()).await.unwrap();
        assert_ne!(s.local_addr.port(), 0);
    }
}
