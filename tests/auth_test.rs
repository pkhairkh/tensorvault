//! Wave 65 — SCRAM-SHA-256 authentication + TLS support for pgwire.
//!
//! Boots a real turboGP server with `auth_required = true`, seeds a
//! bootstrap admin user, connects via raw TCP, performs the SCRAM-SHA-256
//! handshake (RFC 5802) on the client side, and verifies:
//!
//! 1. `CREATE USER alice WITH PASSWORD 'secret'` issued by the admin
//!    connection registers a new user in the password manager.
//! 2. A second connection authenticating as `alice` with the correct
//!    password completes the handshake and can run a SELECT.
//! 3. A third connection authenticating as `alice` with the wrong
//!    password is rejected with an ErrorResponse.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2;
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use turbogp::engine::QueryEngine;
use turbogp::server::auth::PasswordManager;
use turbogp::server::{Server, ServerConfig};

type HmacSha256 = Hmac<Sha256>;

/// Build a SCRAM-SHA-256 client_first / client_final pair for the given
/// password, given the server_first_message returned by the server.
///
/// Returns `(client_first_message, client_final_message)` where
/// `client_first_message` includes the gs2 header `n,,`.
fn scram_client_messages(
    username: &str,
    password: &str,
    server_first: &str,
) -> (String, String) {
    // Parse server_first: r=<combined_nonce>,s=<salt_b64>,i=<iterations>
    let mut combined_nonce = String::new();
    let mut salt_b64 = String::new();
    let mut iterations: u32 = 4096;
    for part in server_first.split(',') {
        if let Some(rest) = part.strip_prefix("r=") {
            combined_nonce = rest.to_string();
        } else if let Some(rest) = part.strip_prefix("s=") {
            salt_b64 = rest.to_string();
        } else if let Some(rest) = part.strip_prefix("i=") {
            iterations = rest.parse().unwrap_or(4096);
        }
    }
    let salt = B64.decode(salt_b64.as_bytes()).expect("salt b64");

    // Client nonce: a short random-looking string. We use a fixed value
    // for determinism in tests (the server doesn't care what it is as
    // long as it's printable and doesn't contain ',').
    let client_nonce = "clientnonce12345";
    let client_first_bare = format!("n={username},r={client_nonce}");
    let client_first = format!("n,,{client_first_bare}");

    // Derive salted_password = PBKDF2-HMAC-SHA-256(password, salt, iterations).
    let mut salted = [0u8; 32];
    pbkdf2::<HmacSha256>(password.as_bytes(), &salt, iterations, &mut salted);

    // client_key = HMAC-SHA-256(salted_password, "Client Key")
    let mut cmac = <HmacSha256 as Mac>::new_from_slice(&salted).unwrap();
    cmac.update(b"Client Key");
    let client_key: [u8; 32] = cmac.finalize().into_bytes().into();

    // stored_key = SHA-256(client_key)
    let mut h = Sha256::new();
    h.update(client_key);
    let stored_key: [u8; 32] = h.finalize().into();

    // server_key = HMAC-SHA-256(salted_password, "Server Key")
    let mut smac = <HmacSha256 as Mac>::new_from_slice(&salted).unwrap();
    smac.update(b"Server Key");
    let _server_key: [u8; 32] = smac.finalize().into_bytes().into();

    // client_final_without_proof = "c=biws,r=<combined_nonce>"
    let client_final_without_proof = format!("c=biws,r={combined_nonce}");

    // AuthMessage = client_first_bare + "," + server_first + "," + client_final_without_proof
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");

    // client_signature = HMAC-SHA-256(stored_key, AuthMessage)
    let mut sig_mac = <HmacSha256 as Mac>::new_from_slice(&stored_key).unwrap();
    sig_mac.update(auth_message.as_bytes());
    let client_sig: [u8; 32] = sig_mac.finalize().into_bytes().into();

    // client_proof = client_key XOR client_signature
    let mut client_proof = [0u8; 32];
    for i in 0..32 {
        client_proof[i] = client_key[i] ^ client_sig[i];
    }
    let proof_b64 = B64.encode(client_proof);

    let client_final = format!("{client_final_without_proof},p={proof_b64}");
    (client_first, client_final)
}

/// A minimal pgwire client that speaks SCRAM-SHA-256.
struct ScramClient {
    s: TcpStream,
}

impl ScramClient {
    async fn connect(addr: std::net::SocketAddr) -> std::io::Result<Self> {
        Ok(Self { s: TcpStream::connect(addr).await? })
    }

    /// Send the SSLRequest + StartupMessage (protocol 3.0, with `user`).
    async fn send_startup(&mut self, user: &str) -> std::io::Result<()> {
        // SSLRequest — server should respond 'N' (no TLS).
        self.s.write_all(&8i32.to_be_bytes()).await?;
        self.s.write_all(&80877103i32.to_be_bytes()).await?;
        self.s.flush().await?;
        let mut b = [0u8; 1];
        self.s.read_exact(&mut b).await?;
        assert_eq!(b[0], b'N', "server should decline SSL when tls is None");
        // StartupMessage
        let mut body = Vec::new();
        body.extend_from_slice(&196608i32.to_be_bytes());
        body.extend_from_slice(b"user\0");
        body.extend_from_slice(user.as_bytes());
        body.push(0);
        body.push(0); // terminator
        self.s.write_all(&((body.len() + 4) as i32).to_be_bytes()).await?;
        self.s.write_all(&body).await?;
        self.s.flush().await
    }

    async fn read_msg(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut h = [0u8; 5];
        self.s.read_exact(&mut h).await?;
        let t = h[0];
        let len = i32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        self.s.read_exact(&mut body).await?;
        Ok((t, body))
    }

    /// Drive the SCRAM-SHA-256 handshake. Returns Ok(()) if the server
    /// sent AuthenticationOk + ReadyForQuery; returns Err with the
    /// server's error message if the server sent an ErrorResponse.
    async fn do_scram(&mut self, user: &str, password: &str) -> std::io::Result<()> {
        // Expect AuthenticationSASL (R, code 10) with mechanism list.
        let (t, body) = self.read_msg().await?;
        assert_eq!(t, b'R', "expected AuthenticationSASL (R), got {t:#x}");
        let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        assert_eq!(code, 10, "expected SASL code 10, got {code}");
        // The rest is a list of cstrings; verify SCRAM-SHA-256 is offered.
        let rest = &body[4..];
        let mut found = false;
        let mut i = 0;
        while i < rest.len() && rest[i] != 0 {
            let end = rest[i..].iter().position(|&b| b == 0).unwrap();
            let mech = std::str::from_utf8(&rest[i..i + end]).unwrap_or("");
            if mech == "SCRAM-SHA-256" { found = true; }
            i += end + 1;
        }
        assert!(found, "server must offer SCRAM-SHA-256");

        // Build client_first_message: "n,,n=user,r=clientnonce"
        // (We use a placeholder server_first here; the real one comes next.)
        let client_nonce = "clientnonce12345";
        let client_first_bare = format!("n={user},r={client_nonce}");
        let client_first = format!("n,,{client_first_bare}");

        // Send SASLInitialResponse: 'p' + len + cstring(mech) + i32(ir_len) + ir_bytes
        let mut payload = Vec::new();
        payload.extend_from_slice(b"SCRAM-SHA-256\0");
        payload.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
        payload.extend_from_slice(client_first.as_bytes());
        self.send_p_message(&payload).await?;

        // Expect AuthenticationSASLContinue (R, code 11) with server_first.
        let (t, body) = self.read_msg().await?;
        if t == b'E' {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                parse_err(&body),
            ));
        }
        assert_eq!(t, b'R', "expected AuthenticationSASLContinue (R), got {t:#x}");
        let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        assert_eq!(code, 11, "expected SASLContinue code 11, got {code}");
        let server_first = std::str::from_utf8(&body[4..]).unwrap_or("");

        // Compute client_final_message.
        let (_cf2, client_final) = scram_client_messages(user, password, server_first);

        // Send SASLResponse: 'p' + len + bytes(client_final).
        self.send_p_message(client_final.as_bytes()).await?;

        // Expect either AuthenticationSASLFinal (R, code 12) + AuthenticationOk
        // (R, code 0), or an ErrorResponse.
        let (t, body) = self.read_msg().await?;
        if t == b'E' {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                parse_err(&body),
            ));
        }
        assert_eq!(t, b'R', "expected AuthenticationSASLFinal (R), got {t:#x}");
        let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        assert_eq!(code, 12, "expected SASLFinal code 12, got {code}");

        // Drain messages until ReadyForQuery ('Z').
        loop {
            let (t, body) = self.read_msg().await?;
            match t {
                b'R' => {
                    let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    assert_eq!(code, 0, "expected AuthenticationOk (0) after SASLFinal, got {code}");
                }
                b'S' | b'K' => {}
                b'Z' => return Ok(()),
                b'E' => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        parse_err(&body),
                    ));
                }
                _ => {}
            }
        }
    }

    /// Send a 'p' (Password) message with the given payload.
    async fn send_p_message(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let len = (payload.len() as i32 + 4).to_be_bytes();
        self.s.write_all(b"p").await?;
        self.s.write_all(&len).await?;
        self.s.write_all(payload).await?;
        self.s.flush().await
    }

    /// Send a simple query (Q message).
    async fn send_query(&mut self, sql: &str) -> std::io::Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        self.s.write_all(b"Q").await?;
        self.s.write_all(&((body.len() as i32 + 4).to_be_bytes())).await?;
        self.s.write_all(&body).await?;
        self.s.flush().await
    }

    /// Read messages until ReadyForQuery ('Z'). Returns the error message
    /// if the server sent an ErrorResponse, or Ok(tag_list) with the
    /// CommandComplete tags seen.
    async fn drain_until_ready(&mut self) -> std::io::Result<Vec<String>> {
        let mut tags = Vec::new();
        loop {
            let (t, body) = self.read_msg().await?;
            match t {
                b'T' | b'D' | b'S' | b'K' | b'R' | b'n' => {}
                b'C' => {
                    let s = std::str::from_utf8(&body[..body.len().saturating_sub(1)])
                        .unwrap_or("")
                        .to_string();
                    tags.push(s);
                }
                b'E' => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        parse_err(&body),
                    ));
                }
                b'Z' => return Ok(tags),
                _ => {}
            }
        }
    }
}

fn parse_err(body: &[u8]) -> String {
    let mut i = 0;
    let mut msg = String::new();
    while i < body.len() && body[i] != 0 {
        let f = body[i] as char;
        i += 1;
        let end = body[i..].iter().position(|&b| b == 0).unwrap_or(body.len() - i);
        if f == 'M' {
            msg = String::from_utf8_lossy(&body[i..i + end]).into_owned();
        }
        i += end + 1;
    }
    msg
}

/// Build an engine with a small test table for SELECT queries.
fn make_engine() -> QueryEngine {
    let mut e = QueryEngine::new();
    e.execute("CREATE TABLE t (id INT)").expect("create table");
    e.execute("INSERT INTO t (id) VALUES (1), (2), (3)").expect("insert");
    e
}

/// Boot a server with auth_required=true and the given password manager.
async fn boot_auth_server(
    engine: QueryEngine,
    passwords: Arc<RwLock<PasswordManager>>,
) -> std::net::SocketAddr {
    let engine = Arc::new(RwLock::new(engine));
    let mut config = ServerConfig::default();
    config.auth_required = true;
    config.passwords = passwords;
    let s = Server::bind(engine, config).await.unwrap();
    let a = s.local_addr;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    a
}

/// Boot a server with auth_required=false (the default) and the given
/// password manager. Used for the CREATE USER test step.
async fn boot_noauth_server(
    engine: QueryEngine,
    passwords: Arc<RwLock<PasswordManager>>,
) -> std::net::SocketAddr {
    let engine = Arc::new(RwLock::new(engine));
    let mut config = ServerConfig::default();
    config.auth_required = false;
    config.passwords = passwords;
    let s = Server::bind(engine, config).await.unwrap();
    let a = s.local_addr;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    a
}

/// The "trust auth" path (auth_required = false) still works: a client
/// that does NOT do SCRAM can connect and run queries. This guards
/// against regressions where the new auth code breaks the default path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noauth_still_works() {
    let passwords = Arc::new(RwLock::new(PasswordManager::new()));
    let addr = boot_noauth_server(make_engine(), passwords).await;
    let mut c = ScramClient::connect(addr).await.unwrap();
    c.send_startup("turboGP").await.unwrap();
    // No SCRAM — expect AuthenticationOk immediately.
    let (t, body) = c.read_msg().await.unwrap();
    assert_eq!(t, b'R');
    let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    assert_eq!(code, 0, "auth_required=false must send AuthenticationOk (0)");
    c.drain_until_ready().await.unwrap();
    // Run a SELECT.
    c.send_query("SELECT count(*) FROM t").await.unwrap();
    let tags = c.drain_until_ready().await.unwrap();
    assert!(tags.iter().any(|t| t.starts_with("SELECT")), "tags: {tags:?}");
}

/// The DoD test: create a user via CREATE USER, then connect with the
/// correct password (succeeds) and with the wrong password (fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scram_create_user_then_auth() {
    let passwords = Arc::new(RwLock::new(PasswordManager::new()));

    // Step 1: bootstrap by directly seeding an admin user, so we can
    // authenticate and run CREATE USER via pgwire.
    {
        let mut mgr = passwords.write().unwrap();
        mgr.create_user("admin", "admin-password");
    }

    let addr = boot_auth_server(make_engine(), Arc::clone(&passwords)).await;

    // Step 2: connect as admin, run CREATE USER alice WITH PASSWORD 'secret'.
    {
        let mut admin = ScramClient::connect(addr).await.unwrap();
        admin.send_startup("admin").await.unwrap();
        admin.do_scram("admin", "admin-password").await.unwrap();
        admin.send_query("CREATE USER alice WITH PASSWORD 'secret'").await.unwrap();
        let tags = admin.drain_until_ready().await.unwrap();
        assert!(
            tags.iter().any(|t| t == "CREATE USER"),
            "CREATE USER must return tag 'CREATE USER', got {tags:?}"
        );
    }

    // Verify alice was registered in the password manager.
    {
        let mgr = passwords.read().unwrap();
        assert!(mgr.exists("alice"), "alice must exist after CREATE USER");
    }

    // Step 3: connect as alice with the correct password → succeeds.
    {
        let mut alice = ScramClient::connect(addr).await.unwrap();
        alice.send_startup("alice").await.unwrap();
        alice.do_scram("alice", "secret").await.expect("correct password must succeed");
        // Run a SELECT to confirm the session is usable.
        alice.send_query("SELECT count(*) FROM t").await.unwrap();
        let tags = alice.drain_until_ready().await.expect("SELECT must succeed");
        assert!(
            tags.iter().any(|t| t.starts_with("SELECT")),
            "SELECT after auth must succeed, got {tags:?}"
        );
    }

    // Step 4: connect as alice with the WRONG password → fails.
    {
        let mut alice = ScramClient::connect(addr).await.unwrap();
        alice.send_startup("alice").await.unwrap();
        let result = alice.do_scram("alice", "wrong-password").await;
        assert!(
            result.is_err(),
            "wrong password must fail the SCRAM handshake, got: {result:?}"
        );
        let err = result.unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("password") || msg.contains("auth") || msg.contains("failed"),
            "error message should mention password/auth, got: {msg}"
        );
    }
}

/// DROP USER removes the user from the password manager. After DROP,
/// a SCRAM handshake for that user must fail (unknown user).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_user_removes_credentials() {
    let passwords = Arc::new(RwLock::new(PasswordManager::new()));
    {
        let mut mgr = passwords.write().unwrap();
        mgr.create_user("admin", "admin-password");
        mgr.create_user("bob", "bob-password");
    }
    let addr = boot_auth_server(make_engine(), Arc::clone(&passwords)).await;

    // Connect as admin and DROP USER bob.
    {
        let mut admin = ScramClient::connect(addr).await.unwrap();
        admin.send_startup("admin").await.unwrap();
        admin.do_scram("admin", "admin-password").await.unwrap();
        admin.send_query("DROP USER bob").await.unwrap();
        admin.drain_until_ready().await.unwrap();
    }
    {
        let mgr = passwords.read().unwrap();
        assert!(!mgr.exists("bob"), "bob must be gone after DROP USER");
    }

    // Connecting as bob must now fail.
    {
        let mut bob = ScramClient::connect(addr).await.unwrap();
        bob.send_startup("bob").await.unwrap();
        let result = bob.do_scram("bob", "bob-password").await;
        assert!(result.is_err(), "dropped user must not authenticate");
    }
}

/// `DROP USER IF EXISTS` on a non-existent user is a no-op (returns Ok).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_user_if_exists_is_safe() {
    let passwords = Arc::new(RwLock::new(PasswordManager::new()));
    {
        let mut mgr = passwords.write().unwrap();
        mgr.create_user("admin", "admin-password");
    }
    let addr = boot_auth_server(make_engine(), Arc::clone(&passwords)).await;
    let mut admin = ScramClient::connect(addr).await.unwrap();
    admin.send_startup("admin").await.unwrap();
    admin.do_scram("admin", "admin-password").await.unwrap();
    admin.send_query("DROP USER IF EXISTS ghost").await.unwrap();
    let tags = admin.drain_until_ready().await.unwrap();
    assert!(tags.iter().any(|t| t == "DROP USER"), "DROP USER IF EXISTS must succeed, got {tags:?}");
}
