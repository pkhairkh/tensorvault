//! Postgres v3 wire protocol implementation.
//!
//! Message framing: every backend message = 1 byte type + 4 byte BE length
//! (length includes itself, excludes type byte) + payload. Frontend messages
//! have the same format (except startup, which has no type byte).

use super::session::{Session, TxnStatus};
use crate::engine::{QueryEngine, QueryResult};
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

const SSL_REQUEST_MAGIC: i32 = 80877103;
const GSSAPI_REQUEST_MAGIC: i32 = 80877104;
const CANCEL_REQUEST_MAGIC: i32 = 80877102;
const PROTOCOL_3_0: i32 = 196608;

#[derive(Debug, Clone)]
struct PreparedStatement {
    sql: String,
    param_oids: Vec<u32>,
}

#[derive(Debug, Clone)]
struct Portal {
    stmt_name: String,
    params: Vec<String>,
    result_formats: Vec<i16>,
}

pub struct PgConn {
    stream_read: BufReader<OwnedReadHalf>,
    stream_write: BufWriter<OwnedWriteHalf>,
    session: Session,
    statements: HashMap<String, PreparedStatement>,
    portals: HashMap<String, Portal>,
}

impl PgConn {
    /// Drive one connection to completion.
    pub async fn handle(
        stream: tokio::net::TcpStream,
        peer: std::net::SocketAddr,
        engine: Arc<Mutex<QueryEngine>>,
        _server_name: String,
    ) -> io::Result<()> {
        let _ = peer;
        let _ = stream.set_nodelay(true);
        let (rh, wh) = stream.into_split();
        let mut conn = PgConn {
            stream_read: BufReader::with_capacity(8 * 1024, rh),
            stream_write: BufWriter::with_capacity(8 * 1024, wh),
            session: Session::new(),
            statements: HashMap::new(),
            portals: HashMap::new(),
        };
        let result = conn.run_loop(&engine).await;
        if let Err(e) = &result {
            log::debug!("pgwire conn closed: {e}");
        }
        result
    }

    async fn run_loop(&mut self, engine: &Arc<Mutex<QueryEngine>>) -> io::Result<()> {
        self.handle_startup().await?;
        loop {
            self.flush().await?;
            let msg_type = match self.read_byte().await {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };
            let len = self.read_i32_be().await? as usize;
            if len < 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("msg {msg_type:#x} len {len} < 4")));
            }
            let body_len = len - 4;
            match msg_type {
                b'Q' => {
                    let sql = self.read_string(body_len).await?;
                    self.handle_simple_query(engine, &sql).await?;
                }
                b'P' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_parse(&buf).await?;
                }
                b'B' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_bind(&buf).await?;
                }
                b'D' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_describe(engine, &buf).await?;
                }
                b'E' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_execute(engine, &buf).await?;
                }
                b'S' => {
                    self.flush().await?;
                    self.send_ready_for_query().await?;
                }
                b'C' => {
                    let buf = self.read_body(body_len).await?;
                    self.handle_close(&buf);
                    self.send_byte(b'3', &[]).await?;
                }
                b'H' => { self.flush().await?; }
                b'X' => return Ok(()),
                other => {
                    if body_len > 0 {
                        let mut sink = vec![0u8; body_len];
                        self.stream_read.read_exact(&mut sink).await?;
                    }
                    let _ = self.send_error("0A000", &format!("unsupported msg {other:#x}")).await;
                }
            }
        }
    }

    // --- Startup ---

    async fn handle_startup(&mut self) -> io::Result<()> {
        loop {
            let len = self.read_i32_be().await?;
            if !(4..=1_000_000).contains(&len) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("startup len {len}")));
            }
            let body_len = (len - 4) as usize;
            let mut buf = vec![0u8; body_len];
            self.stream_read.read_exact(&mut buf).await?;
            if buf.len() < 4 { return Err(io::Error::new(io::ErrorKind::InvalidData, "startup too short")); }
            let magic = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
            match magic {
                SSL_REQUEST_MAGIC | GSSAPI_REQUEST_MAGIC => {
                    self.stream_write.write_all(b"N").await?;
                    self.flush().await?;
                    continue;
                }
                CANCEL_REQUEST_MAGIC => return Ok(()),
                m if (196608..=196620).contains(&m) => {
                    self.finish_startup_v3(&buf).await?;
                    self.flush().await?;
                    return Ok(());
                }
                _ => {
                    let _ = self.send_error("08P01", "unsupported protocol").await;
                    self.flush().await?;
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("magic {magic}")));
                }
            }
        }
    }

    async fn finish_startup_v3(&mut self, buf: &[u8]) -> io::Result<()> {
        let rest = &buf[4..];
        for (k, v) in parse_cstring_pairs(rest) {
            match k.as_str() {
                "user" => self.session.user = Some(v),
                "database" => self.session.database = Some(v),
                "application_name" => self.session.application_name = Some(v),
                _ => {}
            }
        }
        if self.session.user.is_none() { self.session.user = Some("turboGP".into()); }

        // AuthenticationOk
        self.send_byte(b'R', &0u32.to_be_bytes()).await?;
        // ParameterStatus messages
        self.send_parameter_status("server_version", "15.0").await?;
        self.send_parameter_status("server_encoding", "UTF8").await?;
        self.send_parameter_status("client_encoding", "UTF8").await?;
        self.send_parameter_status("DateStyle", "ISO, MDY").await?;
        self.send_parameter_status("integer_datetimes", "on").await?;
        self.send_parameter_status("standard_conforming_strings", "on").await?;
        self.send_parameter_status("application_name", "turboGP").await?;
        self.send_parameter_status("IntervalStyle", "postgres").await?;
        self.send_parameter_status("TimeZone", "UTC").await?;
        // BackendKeyData: process_id (4) + secret_key (4) = 8 bytes
        let pid: i32 = rand_backend_key();
        let key: i32 = rand_backend_key();
        let mut kb = Vec::with_capacity(8);
        kb.extend_from_slice(&pid.to_be_bytes());
        kb.extend_from_slice(&key.to_be_bytes());
        self.send_byte(b'K', &kb).await?;
        // ReadyForQuery
        self.send_ready_for_query().await
    }

    // --- Simple query ---

    async fn handle_simple_query(&mut self, engine: &Arc<Mutex<QueryEngine>>, sql: &str) -> io::Result<()> {
        let stmts = split_sql_batch(sql);
        let was_txn = self.session.txn != TxnStatus::Idle;
        for stmt in stmts {
            let trimmed = stmt.trim();
            if trimmed.is_empty() { continue; }
            let lower = trimmed.to_lowercase();
            if lower.starts_with("begin") || lower.starts_with("start transaction") {
                self.session.txn = TxnStatus::InTransaction;
                self.send_command_complete("BEGIN", 0).await?;
                continue;
            }
            if lower.starts_with("commit") {
                self.session.txn = TxnStatus::Idle;
                self.send_command_complete("COMMIT", 0).await?;
                continue;
            }
            if lower.starts_with("rollback") {
                self.session.txn = TxnStatus::Idle;
                self.send_command_complete("ROLLBACK", 0).await?;
                continue;
            }
            let result = {
                let mut guard = engine.lock().expect("engine mutex");
                guard.execute(trimmed)
            };
            match result {
                Ok(r) => {
                    self.send_row_description(&r).await?;
                    self.send_data_rows(&r).await?;
                    self.send_command_complete(&command_tag(&r, trimmed), r.row_count).await?;
                }
                Err(e) => {
                    let _ = self.send_error("42000", &format!("{e}")).await;
                    if was_txn { self.session.txn = TxnStatus::FailedTransaction; }
                    break;
                }
            }
        }
        self.send_ready_for_query().await
    }

    // --- Extended query ---

    async fn handle_parse(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut c = 0;
        let name = read_cstring(buf, &mut c)?;
        let sql = read_cstring(buf, &mut c)?;
        if c + 2 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Parse truncated")); }
        let n = u16::from_be_bytes([buf[c], buf[c+1]]) as usize;
        c += 2;
        let mut oids = Vec::with_capacity(n);
        for _ in 0..n {
            if c + 4 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Parse OID truncated")); }
            oids.push(u32::from_be_bytes([buf[c],buf[c+1],buf[c+2],buf[c+3]]));
            c += 4;
        }
        self.statements.insert(name, PreparedStatement { sql, param_oids: oids });
        self.send_byte(b'1', &[]).await // ParseComplete
    }

    async fn handle_bind(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut c = 0;
        let portal_name = read_cstring(buf, &mut c)?;
        let stmt_name = read_cstring(buf, &mut c)?;
        if c + 2 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind truncated")); }
        let n_fmt = u16::from_be_bytes([buf[c], buf[c+1]]) as usize;
        c += 2;
        let mut pfmts = Vec::with_capacity(n_fmt);
        for _ in 0..n_fmt {
            if c + 2 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind fmt truncated")); }
            pfmts.push(i16::from_be_bytes([buf[c], buf[c+1]]));
            c += 2;
        }
        if c + 2 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind truncated")); }
        let n_params = u16::from_be_bytes([buf[c], buf[c+1]]) as usize;
        c += 2;
        let mut params = Vec::with_capacity(n_params);
        for i in 0..n_params {
            if c + 4 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind param truncated")); }
            let plen = i32::from_be_bytes([buf[c],buf[c+1],buf[c+2],buf[c+3]]);
            c += 4;
            let val = if plen < 0 { None }
            else {
                let plen = plen as usize;
                if c + plen > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind param overflow")); }
                let bytes = &buf[c..c+plen];
                c += plen;
                let fmt = if pfmts.len() == 1 { pfmts[0] } else if pfmts.len() > i { pfmts[i] } else { 0 };
                if fmt == 0 { Some(String::from_utf8_lossy(bytes).into_owned()) }
                else { Some(format!("\\x{}", hex_encode(bytes))) }
            };
            params.push(val.unwrap_or_else(|| "NULL".into()));
        }
        if c + 2 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind rfmt truncated")); }
        let n_rfmt = u16::from_be_bytes([buf[c], buf[c+1]]) as usize;
        c += 2;
        let mut rfmts = Vec::with_capacity(n_rfmt);
        for _ in 0..n_rfmt {
            if c + 2 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Bind rfmt truncated")); }
            rfmts.push(i16::from_be_bytes([buf[c], buf[c+1]]));
            c += 2;
        }
        if !self.statements.contains_key(&stmt_name) {
            let _ = self.send_error("26000", &format!("prepared statement \"{stmt_name}\" does not exist")).await;
            return Ok(());
        }
        self.portals.insert(portal_name, Portal { stmt_name, params, result_formats: rfmts });
        self.send_byte(b'2', &[]).await // BindComplete
    }

    async fn handle_describe(&mut self, engine: &Arc<Mutex<QueryEngine>>, buf: &[u8]) -> io::Result<()> {
        if buf.is_empty() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Describe empty")); }
        let kind = buf[0];
        let mut c = 1;
        let name = read_cstring(buf, &mut c)?;
        match kind {
            b'S' => {
                let stmt = match self.statements.get(&name) { Some(s) => s.clone(), None => {
                    let _ = self.send_error("26000", &format!("statement \"{name}\" not found")).await;
                    return Ok(());
                }};
                let n = stmt.param_oids.len() as u16;
                let mut body = Vec::with_capacity(2 + stmt.param_oids.len() * 4);
                body.extend_from_slice(&n.to_be_bytes());
                for oid in &stmt.param_oids { body.extend_from_slice(&oid.to_be_bytes()); }
                self.send_byte(b't', &body).await?; // ParameterDescription
                self.send_byte(b'n', &[]).await?; // NoData (schema unknown until V-3)
            }
            b'P' => {
                let portal = match self.portals.get(&name) { Some(p) => p.clone(), None => {
                    let _ = self.send_error("34000", &format!("portal \"{name}\" not found")).await;
                    return Ok(());
                }};
                let stmt = match self.statements.get(&portal.stmt_name) { Some(s) => s.clone(), None => {
                    let _ = self.send_error("26000", &format!("statement \"{}\" not found", portal.stmt_name)).await;
                    return Ok(());
                }};
                let sql = substitute_params(&stmt.sql, &portal.params);
                let result = { let mut g = engine.lock().expect("engine"); g.execute(&sql) };
                match result {
                    Ok(r) => {
                        if r.columns.is_empty() { self.send_byte(b'n', &[]).await?; }
                        else { self.send_row_description(&r).await?; }
                    }
                    Err(_) => { self.send_byte(b'n', &[]).await?; }
                }
            }
            _ => { let _ = self.send_error("08P01", "unknown describe kind").await; }
        }
        Ok(())
    }

    async fn handle_execute(&mut self, engine: &Arc<Mutex<QueryEngine>>, buf: &[u8]) -> io::Result<()> {
        let mut c = 0;
        let portal_name = read_cstring(buf, &mut c)?;
        if c + 4 > buf.len() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Execute truncated")); }
        let _max_rows = i32::from_be_bytes([buf[c],buf[c+1],buf[c+2],buf[c+3]]);
        let portal = match self.portals.get(&portal_name) { Some(p) => p.clone(), None => {
            let _ = self.send_error("34000", &format!("portal \"{portal_name}\" not found")).await;
            return Ok(());
        }};
        let stmt = match self.statements.get(&portal.stmt_name) { Some(s) => s.clone(), None => {
            let _ = self.send_error("26000", &format!("statement \"{}\" not found", portal.stmt_name)).await;
            return Ok(());
        }};
        let sql = substitute_params(&stmt.sql, &portal.params);
        let result = { let mut g = engine.lock().expect("engine"); g.execute(&sql) };
        match result {
            Ok(r) => {
                self.send_data_rows(&r).await?;
                self.send_command_complete(&command_tag(&r, &sql), r.row_count).await?;
            }
            Err(e) => { let _ = self.send_error("42000", &format!("{e}")).await; }
        }
        Ok(())
    }

    fn handle_close(&mut self, buf: &[u8]) {
        if buf.is_empty() { return; }
        let kind = buf[0];
        let mut c = 1;
        if let Ok(name) = read_cstring(buf, &mut c) {
            match kind {
                b'S' => { self.statements.remove(&name); }
                b'P' => { self.portals.remove(&name); }
                _ => {}
            }
        }
    }

    // --- Outbound helpers ---

    async fn send_parameter_status(&mut self, key: &str, val: &str) -> io::Result<()> {
        let mut body = Vec::with_capacity(key.len() + 1 + val.len() + 1);
        body.extend_from_slice(key.as_bytes()); body.push(0);
        body.extend_from_slice(val.as_bytes()); body.push(0);
        self.send_byte(b'S', &body).await
    }

    async fn send_row_description(&mut self, r: &QueryResult) -> io::Result<()> {
        if r.columns.is_empty() { self.send_byte(b'n', &[]).await?; return Ok(()); }
        let mut body = Vec::new();
        body.extend_from_slice(&(r.columns.len() as u16).to_be_bytes());
        for col in &r.columns {
            body.extend_from_slice(col.name.as_bytes()); body.push(0);
            body.extend_from_slice(&0u32.to_be_bytes()); // table OID
            body.extend_from_slice(&0u16.to_be_bytes()); // col attr
            body.extend_from_slice(&20u32.to_be_bytes()); // type OID = int8
            body.extend_from_slice(&8i16.to_be_bytes());  // type size
            body.extend_from_slice(&(-1i32).to_be_bytes()); // type mod
            body.extend_from_slice(&0i16.to_be_bytes()); // format = text
        }
        self.send_byte(b'T', &body).await
    }

    async fn send_data_rows(&mut self, r: &QueryResult) -> io::Result<()> {
        for row_idx in 0..r.row_count {
            let mut body = Vec::new();
            body.extend_from_slice(&(r.columns.len() as u16).to_be_bytes());
            for col in &r.columns {
                let v = col.values.get(row_idx).copied().unwrap_or(0);
                let s = v.to_string();
                body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                body.extend_from_slice(s.as_bytes());
            }
            self.send_byte(b'D', &body).await?;
        }
        Ok(())
    }

    async fn send_command_complete(&mut self, tag: &str, _n: usize) -> io::Result<()> {
        let mut body = Vec::with_capacity(tag.len() + 1);
        body.extend_from_slice(tag.as_bytes()); body.push(0);
        self.send_byte(b'C', &body).await
    }

    async fn send_ready_for_query(&mut self) -> io::Result<()> {
        self.send_byte(b'Z', &[self.session.txn.tag()]).await
    }

    async fn send_error(&mut self, code: &str, msg: &str) -> io::Result<()> {
        let mut body = Vec::new();
        body.push(b'S'); body.extend_from_slice(b"ERROR"); body.push(0);
        body.push(b'V'); body.extend_from_slice(b"ERROR"); body.push(0);
        body.push(b'C'); body.extend_from_slice(code.as_bytes()); body.push(0);
        body.push(b'M'); body.extend_from_slice(msg.as_bytes()); body.push(0);
        body.push(0); // terminator
        self.send_byte(b'E', &body).await
    }

    async fn send_byte(&mut self, byte: u8, body: &[u8]) -> io::Result<()> {
        let len = (body.len() as u32 + 4).to_be_bytes();
        self.stream_write.write_all(&[byte]).await?;
        self.stream_write.write_all(&len).await?;
        self.stream_write.write_all(body).await?;
        Ok(())
    }

    async fn flush(&mut self) -> io::Result<()> { self.stream_write.flush().await }

    async fn read_byte(&mut self) -> io::Result<u8> {
        let mut buf = [0u8; 1];
        self.stream_read.read_exact(&mut buf).await?;
        Ok(buf[0])
    }
    async fn read_i32_be(&mut self) -> io::Result<i32> {
        let mut buf = [0u8; 4];
        self.stream_read.read_exact(&mut buf).await?;
        Ok(i32::from_be_bytes(buf))
    }
    async fn read_string(&mut self, body_len: usize) -> io::Result<String> {
        let mut buf = vec![0u8; body_len];
        self.stream_read.read_exact(&mut buf).await?;
        while buf.last() == Some(&0) { buf.pop(); }
        String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    async fn read_body(&mut self, body_len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; body_len];
        self.stream_read.read_exact(&mut buf).await?;
        Ok(buf)
    }
}

// --- Free functions ---

fn split_sql_batch(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_str = false;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            current.push(c as char);
            if c == b'\'' {
                if i + 1 < bytes.len() && bytes[i+1] == b'\'' { current.push('\''); i += 2; continue; }
                in_str = false;
            }
            i += 1; continue;
        }
        if c == b'\'' { in_str = true; current.push('\''); i += 1; continue; }
        // GO separator
        if (c == b'G' || c == b'g') && i + 1 < bytes.len() && (bytes[i+1] == b'O' || bytes[i+1] == b'o')
            && (i == 0 || bytes[i-1] == b'\n' || bytes[i-1] == b'\r')
        {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') { j += 1; }
            if j == bytes.len() || bytes[j] == b'\n' || bytes[j] == b'\r' {
                if !current.trim().is_empty() { out.push(std::mem::take(&mut current)); }
                i = j; continue;
            }
        }
        if c == b';' {
            if !current.trim().is_empty() { out.push(std::mem::take(&mut current)); }
            i += 1; continue;
        }
        current.push(c as char);
        i += 1;
    }
    if !current.trim().is_empty() { out.push(current); }
    out
}

fn parse_cstring_pairs(buf: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() && buf[i] != 0 {
        let k = match read_cstring(buf, &mut i) { Ok(s) => s, Err(_) => break };
        if i >= buf.len() || buf[i] == 0 { out.push((k, String::new())); break; }
        let v = match read_cstring(buf, &mut i) { Ok(s) => s, Err(_) => break };
        out.push((k, v));
    }
    out
}

fn read_cstring(buf: &[u8], cursor: &mut usize) -> io::Result<String> {
    let end = buf[*cursor..].iter().position(|&b| b == 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing NUL"))?;
    let start = *cursor;
    *cursor = start + end + 1;
    String::from_utf8(buf[start..start+end].to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn command_tag(r: &QueryResult, sql: &str) -> String {
    let lower = sql.trim_start().to_lowercase();
    if lower.starts_with("select") || lower.starts_with("with") { return format!("SELECT {}", r.row_count); }
    if lower.starts_with("insert") { return format!("INSERT 0 {}", r.row_count); }
    if lower.starts_with("update") { return format!("UPDATE {}", r.row_count); }
    if lower.starts_with("delete") { return format!("DELETE {}", r.row_count); }
    if lower.starts_with("create") { return "CREATE".into(); }
    if lower.starts_with("drop") { return "DROP".into(); }
    if lower.starts_with("begin") || lower.starts_with("start transaction") { return "BEGIN".into(); }
    if lower.starts_with("commit") { return "COMMIT".into(); }
    if lower.starts_with("rollback") { return "ROLLBACK".into(); }
    "OK".into()
}

fn substitute_params(sql: &str, params: &[String]) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i+1].is_ascii_digit() {
            let mut j = i + 1;
            let mut n: usize = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() { n = n * 10 + (bytes[j] - b'0') as usize; j += 1; }
            if n >= 1 && n <= params.len() { out.push_str(&params[n-1]); }
            else { out.push_str("NULL"); }
            i = j; continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_encode(b: &[u8]) -> String { b.iter().map(|b| format!("{:02x}", b)).collect() }

fn rand_backend_key() -> i32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i32).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_semicolon() {
        let s = split_sql_batch("SELECT 1; SELECT 2; SELECT 3");
        assert_eq!(s, vec!["SELECT 1", " SELECT 2", " SELECT 3"]);
    }
    #[test]
    fn split_go() {
        let s = split_sql_batch("SELECT 1\nGO\nSELECT 2");
        let t: Vec<String> = s.iter().map(|s| s.trim().to_string()).collect();
        assert_eq!(t, vec!["SELECT 1", "SELECT 2"]);
    }
    #[test]
    fn split_ignores_semicolon_in_string() {
        let s = split_sql_batch("SELECT 'a;b'; SELECT 'c'");
        assert_eq!(s, vec!["SELECT 'a;b'", " SELECT 'c'"]);
    }
    #[test]
    fn split_ignores_go_in_string() {
        let s = split_sql_batch("SELECT 'go'; SELECT 1");
        assert_eq!(s, vec!["SELECT 'go'", " SELECT 1"]);
    }
    #[test]
    fn split_escaped_quote() {
        let s = split_sql_batch("SELECT 'it''s'; SELECT 1");
        assert_eq!(s, vec!["SELECT 'it''s'", " SELECT 1"]);
    }
    #[test]
    fn parse_cstring_pairs_basic() {
        let p = parse_cstring_pairs(b"user\0alice\0database\0test\0\0");
        assert_eq!(p, vec![("user".into(), "alice".into()), ("database".into(), "test".into())]);
    }
    #[test]
    fn read_cstring_basic() {
        let buf = b"hello\0world\0";
        let mut c = 0;
        assert_eq!(read_cstring(buf, &mut c).unwrap(), "hello");
        assert_eq!(read_cstring(buf, &mut c).unwrap(), "world");
    }
    #[test]
    fn substitute_basic() {
        assert_eq!(substitute_params("SELECT $1 + $2", &["42".into(), "100".into()]), "SELECT 42 + 100");
    }
    #[test]
    fn substitute_oob_null() {
        assert_eq!(substitute_params("SELECT $1, $3", &["42".into()]), "SELECT 42, NULL");
    }
    #[test]
    fn command_tag_select() {
        assert_eq!(command_tag(&QueryResult::empty(), "SELECT 1"), "SELECT 0");
    }
    #[test]
    fn hex_basic() { assert_eq!(hex_encode(&[0x01, 0xff]), "01ff"); }
}
