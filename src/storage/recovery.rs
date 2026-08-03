//! # Durability: WAL replay + checkpoint (Wave 14).
//!
//! Implements a write-ahead log (WAL) for DML operations and a checkpoint
//! mechanism that flushes the in-memory catalog to a persistent format.
//! On restart, the WAL is replayed to restore the catalog to its last
//! committed state.
//!
//! The WAL format is simple: each record is a serialized DML statement
//! (INSERT/UPDATE/DELETE) with a transaction ID and commit marker. Records
//! are appended to a file and fsync'd on commit.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// A WAL record: one DML operation.
#[derive(Debug, Clone)]
pub struct WalRecord {
    /// Transaction ID (0 for autocommit).
    pub txn_id: u64,
    /// The SQL statement.
    pub sql: String,
    /// True if this is a commit marker for the transaction.
    pub is_commit: bool,
    /// True if this is a rollback marker.
    pub is_rollback: bool,
}

/// The WAL: appends records to a file and provides a reader for replay.
pub struct Wal {
    path: String,
    file: Option<File>,
}

impl Wal {
    /// Open (or create) a WAL at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        Ok(Wal { path: path_str, file: Some(file) })
    }

    /// Append a record to the WAL. Does NOT fsync — call `sync()` to
    /// durably persist.
    pub fn append(&mut self, record: &WalRecord) -> std::io::Result<()> {
        if let Some(ref mut file) = self.file {
            // Format: txn_id|commit|rollback|sql\n
            let line = format!(
                "{}|{}|{}|{}\n",
                record.txn_id,
                if record.is_commit { 1 } else { 0 },
                if record.is_rollback { 1 } else { 0 },
                record.sql.replace('|', "\\|").replace('\n', "\\n")
            );
            file.write_all(line.as_bytes())?;
        }
        Ok(())
    }

    /// Fsync the WAL file to durably persist all appended records.
    pub fn sync(&mut self) -> std::io::Result<()> {
        if let Some(ref mut file) = self.file {
            file.sync_all()?;
        }
        Ok(())
    }

    /// Read all records from the WAL (for replay on startup).
    pub fn read_all(&self) -> std::io::Result<Vec<WalRecord>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut records = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() < 4 {
                continue;
            }
            let txn_id: u64 = parts[0].parse().unwrap_or(0);
            let is_commit = parts[1] == "1";
            let is_rollback = parts[2] == "1";
            let sql = parts[3]
                .replace("\\|", "|")
                .replace("\\n", "\n");
            records.push(WalRecord { txn_id, sql, is_commit, is_rollback });
        }
        Ok(records)
    }

    /// Truncate the WAL (after a successful checkpoint).
    pub fn truncate(&mut self) -> std::io::Result<()> {
        // Close the current file, truncate it, and reopen for append.
        self.file = None;
        // First truncate: open with write+truncate.
        {
            let _ = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?;
        }
        // Then reopen for append+read.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)?;
        self.file = Some(file);
        Ok(())
    }

    /// Close the WAL.
    pub fn close(&mut self) {
        self.file = None;
    }
}

/// Replay WAL records against an engine. Only committed transactions
/// are replayed; rolled-back or incomplete transactions are skipped.
pub fn replay_wal(engine: &mut crate::engine::QueryEngine, wal: &Wal) -> std::io::Result<ReplayStats> {
    let records = wal.read_all()?;
    let mut stats = ReplayStats::default();

    // Group records by transaction. txn_id = 0 means autocommit.
    let mut txn_records: HashMap<u64, Vec<&WalRecord>> = HashMap::new();
    let mut autocommit_records: Vec<&WalRecord> = Vec::new();

    for record in &records {
        if record.txn_id == 0 {
            autocommit_records.push(record);
        } else {
            txn_records.entry(record.txn_id).or_default().push(record);
        }
    }

    // Replay autocommit records (each is its own transaction).
    for record in &autocommit_records {
        if record.is_commit || record.is_rollback {
            continue; // Skip markers for autocommit
        }
        match engine.execute(&record.sql) {
            Ok(_) => stats.replayed += 1,
            Err(e) => {
                stats.errors += 1;
                stats.error_messages.push(format!("replay error: {e}"));
            }
        }
    }

    // Replay transactional records: only committed ones.
    for (txn_id, records) in &txn_records {
        // Check if this transaction was committed.
        let committed = records.iter().any(|r| r.is_commit);
        let rolled_back = records.iter().any(|r| r.is_rollback);

        if !committed || rolled_back {
            stats.skipped += 1;
            continue;
        }

        // Begin a transaction, replay the records, commit.
        let _ = engine.execute("BEGIN");
        for record in records {
            if record.is_commit || record.is_rollback {
                continue;
            }
            match engine.execute(&record.sql) {
                Ok(_) => stats.replayed += 1,
                Err(e) => {
                    stats.errors += 1;
                    stats.error_messages.push(format!("replay error: {e}"));
                }
            }
        }
        let _ = engine.execute("COMMIT");
    }

    Ok(stats)
}

/// Statistics from a WAL replay.
#[derive(Debug, Default)]
pub struct ReplayStats {
    /// Number of records successfully replayed.
    pub replayed: usize,
    /// Number of transactions skipped (uncommitted or rolled back).
    pub skipped: usize,
    /// Number of replay errors.
    pub errors: usize,
    /// Error messages (first few).
    pub error_messages: Vec<String>,
}

/// A checkpoint: flushes the catalog's DDL + data to a SQL file that
/// can be re-executed on restart to restore the state without replaying
/// the full WAL.
pub struct Checkpoint;

impl Checkpoint {
    /// Save a checkpoint of the current catalog state to a SQL file.
    /// The file contains:
    /// 1. CREATE TABLE statements for every table.
    /// 2. INSERT statements for every row.
    pub fn save<P: AsRef<Path>>(
        catalog: &crate::catalog::Catalog,
        path: P,
    ) -> std::io::Result<usize> {
        let mut file = File::create(path)?;
        let mut table_count = 0;
        for name in catalog.table_names() {
            if name == "__dummy__" {
                continue;
            }
            if let Some(table) = catalog.get(name) {
                // Write CREATE TABLE.
                let cols: Vec<String> = table
                    .column_names
                    .iter()
                    .map(|c| format!("{c} INT"))
                    .collect();
                writeln!(file, "CREATE TABLE {name} ({});", cols.join(", "))?;
                table_count += 1;

                // Write INSERT statements.
                for row in 0..table.row_count {
                    let vals: Vec<String> = table
                        .columns
                        .iter()
                        .map(|col| col.get(row).copied().unwrap_or(0).to_string())
                        .collect();
                    writeln!(file, "INSERT INTO {name} VALUES ({});", vals.join(", "))?;
                }
            }
        }
        Ok(table_count)
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn wal_append_and_read() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
            txn_id: 0,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.append(&WalRecord {
            txn_id: 1,
            sql: "INSERT INTO t VALUES (2)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.append(&WalRecord {
            txn_id: 1,
            sql: "".into(),
            is_commit: true,
            is_rollback: false,
        }).unwrap();
        wal.sync().unwrap();

        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].sql, "INSERT INTO t VALUES (1)");
        assert_eq!(records[1].txn_id, 1);
        assert!(records[2].is_commit);
    }

    #[test]
    fn wal_truncate() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
            txn_id: 0,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.read_all().unwrap().len(), 1);

        wal.truncate().unwrap();
        assert_eq!(wal.read_all().unwrap().len(), 0);
    }

    #[test]
    fn wal_special_chars() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
            txn_id: 0,
            sql: "INSERT INTO t VALUES ('a|b\nc')".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.sync().unwrap();

        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sql, "INSERT INTO t VALUES ('a|b\nc')");
    }

    #[test]
    fn replay_autocommit() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
            txn_id: 0,
            sql: "CREATE TABLE t (id INT)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.append(&WalRecord {
            txn_id: 0,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.append(&WalRecord {
            txn_id: 0,
            sql: "INSERT INTO t VALUES (2)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.sync().unwrap();

        let mut engine = crate::engine::QueryEngine::new();
        let stats = replay_wal(&mut engine, &wal).unwrap();
        assert_eq!(stats.replayed, 3);
        assert_eq!(stats.errors, 0);

        // Verify the data was restored.
        let r = engine.execute("SELECT count(*) FROM t").unwrap();
        assert_eq!(r.scalar_u64(), Some(2));
    }

    #[test]
    fn replay_skips_uncommitted() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
            txn_id: 0,
            sql: "CREATE TABLE t (id INT)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        // Transaction 1: INSERT but no COMMIT.
        wal.append(&WalRecord {
            txn_id: 1,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.sync().unwrap();

        let mut engine = crate::engine::QueryEngine::new();
        let stats = replay_wal(&mut engine, &wal).unwrap();
        assert_eq!(stats.replayed, 1); // Only the CREATE TABLE
        assert_eq!(stats.skipped, 1); // Transaction 1 was not committed
    }

    #[test]
    fn replay_skips_rolled_back() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path()).unwrap();
        wal.append(&WalRecord {
            txn_id: 0,
            sql: "CREATE TABLE t (id INT)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        // Transaction 1: INSERT + ROLLBACK.
        wal.append(&WalRecord {
            txn_id: 1,
            sql: "INSERT INTO t VALUES (1)".into(),
            is_commit: false,
            is_rollback: false,
        }).unwrap();
        wal.append(&WalRecord {
            txn_id: 1,
            sql: "".into(),
            is_commit: false,
            is_rollback: true,
        }).unwrap();
        wal.sync().unwrap();

        let mut engine = crate::engine::QueryEngine::new();
        let stats = replay_wal(&mut engine, &wal).unwrap();
        assert_eq!(stats.replayed, 1); // Only the CREATE TABLE
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn checkpoint_save() {
        use crate::datasource::parquet::{LoadedColumn, LoadedTable};
        use crate::datasource::Table as DS;
        let mut cat = crate::catalog::Catalog::new();
        let t = DS::from_loaded(LoadedTable {
            name: "users".into(),
            columns: vec![LoadedColumn {
                name: "id".into(),
                cells: vec![1, 2, 3],
                row_count: 3,
                string_search: None,
            }],
            row_count: 3,
        });
        cat.register(t);

        let tmp = NamedTempFile::new().unwrap();
        let count = Checkpoint::save(&cat, tmp.path()).unwrap();
        assert_eq!(count, 1);

        // Read the file and verify it has CREATE TABLE and INSERT statements.
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(content.contains("CREATE TABLE users"));
        assert!(content.contains("INSERT INTO users VALUES (1)"));
        assert!(content.contains("INSERT INTO users VALUES (2)"));
        assert!(content.contains("INSERT INTO users VALUES (3)"));
    }
}
