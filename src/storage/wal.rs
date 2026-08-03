//! **NOT WIRED INTO SQL EXECUTION** — this module exists but is not called by QueryEngine::execute() (or is only partially wired; see Wave 53 notes in engine/mod.rs).
//! Write-ahead log (WAL) — ZNS-aware (ADR-011).
//!
//! The WAL is append-only and designed for Zoned Namespace SSDs. On a
//! conventional NVMe SSD it works fine but pays GC tail latency; on ZNS it
//! gets predictable ~10–30 µs fsync with no write amplification.
//!
//! ## Zone management (ADR-011)
//!
//! A ZNS SSD exposes *zones* that must be written sequentially. This module
//! tracks zone state via [`WalZone`] and exposes [`Wal::open_zone`] /
//! [`Wal::finish_zone`] / [`Wal::rotate`] for explicit zone management. On a
//! regular file (non-ZNS), zone management is logical bookkeeping — the
//! underlying I/O is plain `BufWriter<File>::write_all`. The [`Wal::append`]
//! method auto-rotates when a zone fills up.
//!
//! ## Crash recovery
//!
//! [`Wal::sync`] is the durability boundary: only records that have been
//! `sync()`'d are guaranteed to survive a process crash. The
//! [`Wal::simulate_crash`] helper consumes the WAL *without* flushing its
//! in-memory buffer, letting tests verify that unsynced records are invisible
//! after reopen. The companion [`WalReader`] iterates the on-disk records.

use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3;

use crate::memory::tier::MemoryTier;

// ---------------------------------------------------------------------------
// Linux ZNS ioctls (ADR-011)
// ---------------------------------------------------------------------------

/// `BLKGETZONESZ` ioctl number on Linux.
///
/// Per ADR-011, this is used to detect ZNS devices at startup. The value
/// `0x40041271` encodes: direction = `_IOC_WRITE` (host→device), size = 4
/// bytes, type = `0x12` (block layer), nr = `0x71`.
#[cfg(target_os = "linux")]
const BLKGETZONESZ: libc::c_ulong = 0x4004_1271;

/// `BLKOPENZONE` ioctl number on Linux (ZNS zone management).
///
/// Takes a `struct blk_zone_range { __u64 sector; __u64 nr_sectors; }`
/// (16 bytes). Encoded as `_IOW(0x12, 0x16, struct blk_zone_range)` =
/// `0x40101216`.
#[cfg(target_os = "linux")]
const BLKOPENZONE: libc::c_ulong = 0x4010_1216;

/// `BLKFINISHZONE` ioctl number on Linux (ZNS zone management).
///
/// Takes a `struct blk_zone_range` (16 bytes). Encoded as
/// `_IOW(0x12, 0x17, struct blk_zone_range)` = `0x40101217`.
#[cfg(target_os = "linux")]
const BLKFINISHZONE: libc::c_ulong = 0x4010_1217;

/// Default zone capacity for non-ZNS devices (1 GB).
///
/// Effectively never triggers auto-rotation in tests; on real ZNS hardware
/// the actual zone size is queried from the device (a future wave will wire
/// the queried size into [`Wal::open`]).
const DEFAULT_ZONE_CAPACITY: u64 = 1 << 30;

/// Detect whether the given path is a ZNS (Zoned Namespace SSD) block device.
///
/// Returns `true` if the path is a Linux block device that supports the
/// `BLKGETZONESZ` ioctl. Returns `false` on non-Linux platforms, on
/// non-block-device paths (regular files, directories, pipes, etc.), or when
/// the ioctl fails (`ENOTTY`, `ENOTBLK`, etc.).
pub fn detect_zns(path: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        // Open the path read-only. We add `O_NONBLOCK` in case the path is a
        // pipe or special file that would block on `open(2)`. For a regular
        // block device this flag is harmless.
        let file = match OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path) {
            Ok(f) => f,
            Err(_) => return false,
        };

        let mut zone_size: u32 = 0;
        // SAFETY: `file` is an open file descriptor obtained from `open(2)`.
        // `BLKGETZONESZ` is a read-only ioctl that writes a `__u32` into the
        // caller-supplied buffer. The pointer is to a stack-allocated `u32`
        // that outlives the call. The ioctl returns 0 on success and -1 on
        // error (with `errno` set); we check the return value.
        let ret =
            unsafe { libc::ioctl(file.as_raw_fd(), BLKGETZONESZ, &mut zone_size as *mut u32) };
        // Drop the file (closes the fd) before returning.
        drop(file);
        ret == 0 && zone_size > 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        // ZNS is a Linux-only NVMe feature; on other platforms we always
        // fall back to regular file append.
        let _ = path;
        false
    }
}

// ---------------------------------------------------------------------------
// WalRecord
// ---------------------------------------------------------------------------

/// A WAL record.
#[derive(Debug, Clone)]
pub struct WalRecord {
    /// Transaction ID.
    pub txn_id: u64,
    /// Record type (0 = commit, 1 = abort, 2 = data).
    pub record_type: u8,
    /// Payload bytes.
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// WalZone
// ---------------------------------------------------------------------------

/// State of a WAL zone (ADR-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalZoneState {
    /// Zone is empty (no data written).
    Empty,
    /// Zone is open and accepting writes.
    Open,
    /// Zone is full (capacity reached); must be finished before reuse.
    Full,
    /// Zone is finished (no more writes possible; must be reset to reuse).
    Finished,
}

/// Tracks the state of a single WAL zone (ADR-011).
///
/// On a ZNS device, a zone corresponds to a physical NVMe zone that must be
/// written sequentially. On a regular file, a "zone" is a logical segment
/// within the file — the zone capacity is enforced in software and the
/// underlying I/O is plain `BufWriter<File>::write_all`.
#[derive(Debug, Clone)]
pub struct WalZone {
    /// Zone ID (sequential, starting from 0).
    pub zone_id: u64,
    /// Capacity in bytes (the maximum bytes that can be written to this zone).
    pub capacity: u64,
    /// Current write offset within the zone (bytes written so far).
    pub write_offset: u64,
    /// Zone state.
    pub state: WalZoneState,
}

impl WalZone {
    /// Create a new empty zone with the given ID and capacity.
    fn new(zone_id: u64, capacity: u64) -> Self {
        Self { zone_id, capacity, write_offset: 0, state: WalZoneState::Empty }
    }

    /// Bytes remaining in the zone before it fills up.
    fn remaining(&self) -> u64 {
        self.capacity.saturating_sub(self.write_offset)
    }
}

// ---------------------------------------------------------------------------
// Wal
// ---------------------------------------------------------------------------

/// A write-ahead log.
pub struct Wal {
    /// Path to the WAL file.
    path: PathBuf,
    /// The underlying file (append-only). `None` after [`Self::simulate_crash`].
    file: Mutex<Option<BufWriter<File>>>,
    /// Bytes written since open (cumulative across zones within this instance).
    bytes_written: Mutex<u64>,
    /// Records written since open.
    records_written: Mutex<u64>,
    /// The tier this WAL writes to (for kernel selection).
    pub tier: MemoryTier,
    /// True if the underlying file is a ZNS block device.
    is_zns: bool,
    /// Capacity of each zone (bytes).
    zone_capacity: u64,
    /// Current zone state, or `None` if no zone is open.
    current_zone: Mutex<Option<WalZone>>,
    /// Number of zones opened since open.
    zones_opened: Mutex<u64>,
    /// Number of zones finished since open.
    zones_finished: Mutex<u64>,
}

impl Wal {
    /// Open or create a WAL at the given path.
    ///
    /// Detects ZNS at open time via [`detect_zns`]. On non-Linux or
    /// non-block-device paths, the WAL operates in regular-file mode.
    pub fn open<P: AsRef<Path>>(path: P, tier: MemoryTier) -> crate::Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let file = OpenOptions::new().create(true).append(true).read(true).open(&path)?;

        // Detect ZNS at open time. On non-Linux or non-block-devices this
        // returns false and the WAL operates in regular-file mode.
        let is_zns = detect_zns(&path.to_string_lossy());
        let zone_capacity = DEFAULT_ZONE_CAPACITY;

        Ok(Self {
            path,
            file: Mutex::new(Some(BufWriter::new(file))),
            bytes_written: Mutex::new(0),
            records_written: Mutex::new(0),
            tier,
            is_zns,
            zone_capacity,
            current_zone: Mutex::new(None),
            zones_opened: Mutex::new(0),
            zones_finished: Mutex::new(0),
        })
    }

    /// Open a new zone for writing (ADR-011).
    ///
    /// On a ZNS device, this issues `ioctl(BLKOPENZONE)` to explicitly open
    /// the next NVMe zone. On a regular file, it just tracks a new logical
    /// zone segment. If a zone is already open, it is finished first.
    ///
    /// Returns the zone ID of the newly opened zone.
    pub fn open_zone(&self) -> crate::Result<u64> {
        // If there's already an open zone, finish it first so we never have
        // two open zones simultaneously (ZNS forbids this).
        if self.current_zone.lock().is_some() {
            self.finish_zone()?;
        }

        let zone_id = {
            let mut opened = self.zones_opened.lock();
            let id = *opened;
            *opened += 1;
            id
        };

        #[cfg(target_os = "linux")]
        if self.is_zns {
            use std::os::unix::io::AsRawFd;
            let guard = self.file.lock();
            if let Some(ref buf) = *guard {
                let file = buf.get_ref();
                // `struct blk_zone_range { __u64 sector; __u64 nr_sectors; }`
                // — 16 bytes. We pass sector = zone_id * (zone_capacity / 512)
                // and nr_sectors = zone_capacity / 512 (ZNS sectors are
                // 512 bytes).
                let zone_size_sectors = self.zone_capacity / 512;
                let sector = zone_id * zone_size_sectors;
                let nr_sectors = zone_size_sectors;
                let range: [u64; 2] = [sector, nr_sectors];
                // SAFETY: `file` is an open file descriptor. `BLKOPENZONE`
                // is a write-only ioctl that reads a 16-byte range from
                // userspace. The pointer is to a stack array that outlives
                // the call. The ioctl returns 0 on success, -1 on error.
                let ret = unsafe {
                    libc::ioctl(file.as_raw_fd(), BLKOPENZONE, &range as *const [u64; 2])
                };
                if ret < 0 {
                    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    return Err(crate::Error::Unsupported(format!(
                        "BLKOPENZONE ioctl failed (errno={errno})"
                    )));
                }
            }
        }

        let zone = WalZone::new(zone_id, self.zone_capacity);
        *self.current_zone.lock() = Some(zone);
        Ok(zone_id)
    }

    /// Finish the current zone (ADR-011).
    ///
    /// On a ZNS device, this issues `ioctl(BLKFINISHZONE)` to mark the zone
    /// as full (no more writes possible until reset). On a regular file, it
    /// syncs the file to create a durable boundary.
    ///
    /// If no zone is currently open, this is a no-op.
    pub fn finish_zone(&self) -> crate::Result<()> {
        let zone = self.current_zone.lock().take();
        let Some(zone) = zone else {
            return Ok(()); // No zone to finish — no-op.
        };

        #[cfg(target_os = "linux")]
        if self.is_zns {
            use std::os::unix::io::AsRawFd;
            let guard = self.file.lock();
            if let Some(ref buf) = *guard {
                let file = buf.get_ref();
                let zone_size_sectors = self.zone_capacity / 512;
                let sector = zone.zone_id * zone_size_sectors;
                let nr_sectors = zone_size_sectors;
                let range: [u64; 2] = [sector, nr_sectors];
                // SAFETY: as in `open_zone` — open fd, write-only ioctl,
                // 16-byte stack array that outlives the call.
                let ret = unsafe {
                    libc::ioctl(file.as_raw_fd(), BLKFINISHZONE, &range as *const [u64; 2])
                };
                if ret < 0 {
                    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    return Err(crate::Error::Unsupported(format!(
                        "BLKFINISHZONE ioctl failed (errno={errno})"
                    )));
                }
            }
        }

        // On non-ZNS, sync to create a durable boundary between zones.
        // On ZNS, the ioctl above already persists the zone state, but a
        // sync is still cheap and harmless (it flushes the kernel page cache).
        self.sync()?;

        *self.zones_finished.lock() += 1;
        Ok(())
    }

    /// Rotate the WAL: finish the current zone and open a new one.
    ///
    /// This is the conventional WAL rotation operation. After `rotate`, the
    /// old zone is durable and a new zone is ready to accept writes.
    pub fn rotate(&self) -> crate::Result<()> {
        self.finish_zone()?;
        self.open_zone()?;
        Ok(())
    }

    /// Append a record to the WAL.
    ///
    /// If no zone is open, opens one automatically. If the current zone
    /// doesn't have enough remaining capacity, finishes it and opens a new
    /// one (auto-rotation per ADR-011).
    ///
    /// Record format:
    /// ```text
    /// [magic "TVW1" (4 bytes)]
    /// [record length (4 bytes, LE)]
    /// [txn_id (8 bytes, LE)]
    /// [record_type (1 byte)]
    /// [payload (variable)]
    /// [checksum (8 bytes, LE)]
    /// ```
    pub fn append(&self, record: &WalRecord) -> crate::Result<()> {
        let mut body = Vec::with_capacity(9 + record.payload.len());
        body.extend_from_slice(&record.txn_id.to_le_bytes());
        body.push(record.record_type);
        body.extend_from_slice(&record.payload);
        let checksum = xxh3::xxh3_64(&body);
        let rec_len = body.len() + 8; // body + checksum

        let mut buf = Vec::with_capacity(8 + rec_len);
        buf.extend_from_slice(b"TVW1");
        buf.extend_from_slice(&(rec_len as u32).to_le_bytes());
        buf.extend_from_slice(&body);
        buf.extend_from_slice(&checksum.to_le_bytes());

        let buf_len = buf.len() as u64;

        // Ensure a zone is open with enough capacity. Auto-rotate if needed.
        let need_rotate = {
            let zone_guard = self.current_zone.lock();
            match &*zone_guard {
                None => true,
                Some(zone) => zone.remaining() < buf_len,
            }
        };
        if need_rotate {
            self.rotate()?;
        }

        // Update the zone's write_offset and state.
        {
            let mut zone_guard = self.current_zone.lock();
            if let Some(ref mut zone) = *zone_guard {
                zone.write_offset += buf_len;
                if zone.state == WalZoneState::Empty {
                    zone.state = WalZoneState::Open;
                }
                if zone.write_offset >= zone.capacity {
                    zone.state = WalZoneState::Full;
                }
            }
        }

        let mut file_guard = self.file.lock();
        let file = file_guard
            .as_mut()
            .ok_or_else(|| crate::Error::Other("WAL has been closed (simulate_crash)".into()))?;
        file.write_all(&buf)?;
        drop(file_guard);

        *self.bytes_written.lock() += buf.len() as u64;
        *self.records_written.lock() += 1;
        Ok(())
    }

    /// Flush the WAL buffer to the OS.
    pub fn flush(&self) -> crate::Result<()> {
        let mut guard = self.file.lock();
        if let Some(ref mut file) = *guard {
            file.flush()?;
        }
        Ok(())
    }

    /// fsync the WAL (durable commit).
    ///
    /// Only records that have been `sync()`'d are guaranteed to survive a
    /// process crash. Unsynchronized records may live only in the
    /// `BufWriter`'s in-memory buffer and will be lost on crash (see
    /// [`Self::simulate_crash`]).
    pub fn sync(&self) -> crate::Result<()> {
        let mut guard = self.file.lock();
        if let Some(ref mut file) = *guard {
            file.flush()?;
            file.get_ref().sync_all()?;
        }
        Ok(())
    }

    /// Bytes written since open.
    pub fn bytes_written(&self) -> u64 {
        *self.bytes_written.lock()
    }

    /// Records written since open.
    pub fn records_written(&self) -> u64 {
        *self.records_written.lock()
    }

    /// Path to the WAL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of zones opened since open.
    pub fn zones_opened(&self) -> u64 {
        *self.zones_opened.lock()
    }

    /// Number of zones finished since open.
    pub fn zones_finished(&self) -> u64 {
        *self.zones_finished.lock()
    }

    /// True if the underlying file is a ZNS block device.
    pub fn is_zns(&self) -> bool {
        self.is_zns
    }

    /// Simulate a process crash: drop the in-memory `BufWriter` buffer
    /// **without** flushing it to the OS, and don't sync.
    ///
    /// This consumes the WAL and is intended for crash-recovery tests. After
    /// this call, any records that were appended but not `sync()`'d are lost
    /// (they were never written to the OS page cache). Only records that were
    /// explicitly `sync()`'d will be visible after reopen.
    ///
    /// # Implementation note
    ///
    /// `BufWriter` auto-flushes on `Drop`. To prevent that, we `take` the
    /// `BufWriter` out of its `Mutex` and `mem::forget` it — skipping the
    /// `Drop` impl entirely. The underlying `File`'s fd is leaked (closed
    /// only on process exit), which is acceptable for tests.
    pub fn simulate_crash(self) {
        let buf = {
            let mut guard = self.file.lock();
            guard.take()
        };
        if let Some(buf) = buf {
            // Forget the BufWriter (and its inner File) without running Drop.
            // This is the only way to prevent BufWriter::flush_buf() from
            // writing the in-memory buffer to the OS.
            std::mem::forget(buf);
        }
    }
}

// ---------------------------------------------------------------------------
// WalReader — iterate on-disk WAL records (for crash recovery / replay)
// ---------------------------------------------------------------------------

/// A reader for the on-disk WAL format.
///
/// Iterates records in the order they were appended. Stops on the first
/// partial or corrupt record (e.g., a record whose checksum doesn't match,
/// indicating a torn write from a crash).
pub struct WalReader {
    file: BufReader<File>,
}

impl WalReader {
    /// Open a WAL file for reading.
    pub fn open<P: AsRef<Path>>(path: P) -> crate::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self { file: BufReader::new(file) })
    }

    /// Read the next record, or `None` at end-of-file (or first corruption).
    ///
    /// A partial trailing record (the result of a crash mid-write) is
    /// treated as end-of-file: the function returns `Ok(None)` without
    /// surfacing an error, because the caller cannot distinguish "no more
    /// records" from "torn tail" without a separate durability marker.
    pub fn next_record(&mut self) -> crate::Result<Option<WalRecord>> {
        // Read the 8-byte header: magic (4) + rec_len (4).
        let mut header = [0u8; 8];
        match self.file.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        if &header[..4] != b"TVW1" {
            // Magic mismatch — either corruption or a partial header at the
            // tail of a torn write. Treat as end-of-stream.
            return Ok(None);
        }
        let rec_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        // rec_len = body.len() + 8 (checksum). Body is at least 9 bytes
        // (txn_id + record_type), so rec_len >= 17.
        if rec_len < 17 {
            return Ok(None); // Corrupt length — stop.
        }

        // Read body + checksum (rec_len bytes total).
        let mut body_and_checksum = vec![0u8; rec_len];
        match self.file.read_exact(&mut body_and_checksum) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let (body, checksum_bytes) = body_and_checksum.split_at(rec_len - 8);
        let stored_checksum = u64::from_le_bytes(checksum_bytes.try_into().unwrap());
        let computed_checksum = xxh3::xxh3_64(body);
        if stored_checksum != computed_checksum {
            // Checksum mismatch — torn write or corruption. Stop iterating.
            return Ok(None);
        }

        let txn_id = u64::from_le_bytes(body[..8].try_into().unwrap());
        let record_type = body[8];
        let payload = body[9..].to_vec();
        Ok(Some(WalRecord { txn_id, record_type, payload }))
    }
}

impl Iterator for WalReader {
    type Item = crate::Result<WalRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn wal_append_and_sync() {
        let dir = tempdir().unwrap();
        let wal = Wal::open(dir.path().join("wal.log"), MemoryTier::Nvme).unwrap();
        wal.append(&WalRecord { txn_id: 1, record_type: 0, payload: b"hello".to_vec() }).unwrap();
        wal.append(&WalRecord { txn_id: 2, record_type: 2, payload: b"world".to_vec() }).unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.records_written(), 2);
        assert!(wal.bytes_written() > 0);
    }

    #[test]
    fn wal_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wal.log");

        {
            let wal = Wal::open(&path, MemoryTier::Nvme).unwrap();
            wal.append(&WalRecord { txn_id: 42, record_type: 0, payload: vec![1, 2, 3] }).unwrap();
            wal.sync().unwrap();
        }

        // Reopen and verify the file exists with content.
        let wal = Wal::open(&path, MemoryTier::Nvme).unwrap();
        // The new WAL should start fresh (we don't replay in this prototype).
        assert_eq!(wal.records_written(), 0);
        // But the file should have content from before.
        let file_size = std::fs::metadata(&path).unwrap().len();
        assert!(file_size > 0);
    }

    // -----------------------------------------------------------------------
    // Wave 8 tests (ADR-011)
    // -----------------------------------------------------------------------

    /// Test 1: ZNS detection on a regular file returns false.
    ///
    /// A regular file is not a block device, so the `BLKGETZONESZ` ioctl
    /// fails (ENOTTY) and `detect_zns` returns false.
    #[test]
    fn detect_zns_returns_false_for_regular_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("not-a-block-device");
        std::fs::write(&path, b"hello").unwrap();
        let path_str = path.to_str().unwrap();
        assert!(!detect_zns(path_str), "regular file must not be detected as ZNS");
    }

    /// Test 1b: ZNS detection on a non-existent path returns false (no panic).
    #[test]
    fn detect_zns_returns_false_for_missing_path() {
        assert!(!detect_zns("/this/path/does/not/exist/at-all"));
    }

    /// Test 1c: a freshly-opened WAL on a regular file is not ZNS.
    #[test]
    fn wal_is_zns_false_on_regular_file() {
        let dir = tempdir().unwrap();
        let wal = Wal::open(dir.path().join("wal.log"), MemoryTier::Nvme).unwrap();
        assert!(!wal.is_zns());
    }

    /// Test 2: WAL append + sync + rotate still works (regression).
    ///
    /// This exercises the full zone lifecycle: append → sync → rotate
    /// → append → sync. Verifies the auto-rotation in `append` doesn't
    /// break the existing append/sync contract.
    #[test]
    fn wal_append_sync_rotate_regression() {
        let dir = tempdir().unwrap();
        let wal = Wal::open(dir.path().join("wal.log"), MemoryTier::Nvme).unwrap();

        // Append + sync.
        wal.append(&WalRecord { txn_id: 1, record_type: 0, payload: b"first".to_vec() }).unwrap();
        wal.append(&WalRecord { txn_id: 2, record_type: 0, payload: b"second".to_vec() }).unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.records_written(), 2);
        assert_eq!(wal.zones_opened(), 1); // First append auto-opened zone 0.

        // Rotate: finish zone 0, open zone 1.
        wal.rotate().unwrap();
        assert_eq!(wal.zones_opened(), 2);
        assert_eq!(wal.zones_finished(), 1);

        // Append to the new zone + sync.
        wal.append(&WalRecord { txn_id: 3, record_type: 0, payload: b"third".to_vec() }).unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.records_written(), 3);

        // File should have content from both zones.
        let file_size = std::fs::metadata(wal.path()).unwrap().len();
        assert!(file_size > 0);
    }

    /// Test 2b: explicit open_zone / finish_zone work without errors on a
    /// regular file.
    #[test]
    fn wal_open_and_finish_zone_on_regular_file() {
        let dir = tempdir().unwrap();
        let wal = Wal::open(dir.path().join("wal.log"), MemoryTier::Nvme).unwrap();

        let zid0 = wal.open_zone().unwrap();
        assert_eq!(zid0, 0);
        wal.append(&WalRecord { txn_id: 1, record_type: 0, payload: b"a".to_vec() }).unwrap();
        wal.finish_zone().unwrap();
        assert_eq!(wal.zones_finished(), 1);

        let zid1 = wal.open_zone().unwrap();
        assert_eq!(zid1, 1);
        wal.append(&WalRecord { txn_id: 2, record_type: 0, payload: b"b".to_vec() }).unwrap();
        wal.finish_zone().unwrap();
        assert_eq!(wal.zones_finished(), 2);
    }

    /// Test 2c: finish_zone with no open zone is a no-op.
    #[test]
    fn wal_finish_zone_noop_when_no_zone() {
        let dir = tempdir().unwrap();
        let wal = Wal::open(dir.path().join("wal.log"), MemoryTier::Nvme).unwrap();
        // No zone opened yet — finish should be a no-op.
        wal.finish_zone().unwrap();
        assert_eq!(wal.zones_finished(), 0);
    }

    /// Test 6: WAL crash simulation — only synced records survive.
    ///
    /// Writes 3 records and syncs them (durable). Then writes 2 more records
    /// WITHOUT syncing, and simulates a process crash via
    /// [`Wal::simulate_crash`] (which forgets the `BufWriter` without
    /// flushing). On reopen, only the 3 synced records are visible.
    #[test]
    fn wal_crash_recovery_only_shows_synced_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("crash.log");

        {
            let wal = Wal::open(&path, MemoryTier::Nvme).unwrap();
            // Write 3 records and sync them — these are durable.
            wal.append(&WalRecord { txn_id: 1, record_type: 0, payload: b"r1".to_vec() }).unwrap();
            wal.append(&WalRecord { txn_id: 2, record_type: 0, payload: b"r2".to_vec() }).unwrap();
            wal.append(&WalRecord { txn_id: 3, record_type: 0, payload: b"r3".to_vec() }).unwrap();
            wal.sync().unwrap();

            // Write 2 more records WITHOUT syncing. These stay in the
            // BufWriter's in-memory buffer and are lost on crash.
            wal.append(&WalRecord { txn_id: 4, record_type: 0, payload: b"r4".to_vec() }).unwrap();
            wal.append(&WalRecord { txn_id: 5, record_type: 0, payload: b"r5".to_vec() }).unwrap();

            // Simulate a crash: forget the BufWriter (and its in-memory
            // buffer of records 4 and 5) without flushing.
            wal.simulate_crash();
        }

        // Reopen and read records. Only the 3 synced records should be visible.
        let mut reader = WalReader::open(&path).unwrap();
        let mut records = Vec::new();
        while let Some(record) = reader.next_record().unwrap() {
            records.push(record);
        }
        assert_eq!(
            records.len(),
            3,
            "only synced records should be visible after a simulated crash"
        );
        assert_eq!(records[0].txn_id, 1);
        assert_eq!(records[1].txn_id, 2);
        assert_eq!(records[2].txn_id, 3);
    }

    /// Test 6b: WalReader reads back synced records correctly (round-trip).
    #[test]
    fn wal_reader_roundtrips_synced_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.log");

        {
            let wal = Wal::open(&path, MemoryTier::Nvme).unwrap();
            wal.append(&WalRecord { txn_id: 10, record_type: 2, payload: vec![0xAA, 0xBB] })
                .unwrap();
            wal.append(&WalRecord { txn_id: 20, record_type: 0, payload: vec![] }).unwrap();
            wal.append(&WalRecord {
                txn_id: 30,
                record_type: 1,
                payload: b"abort-reason".to_vec(),
            })
            .unwrap();
            wal.sync().unwrap();
        }

        let reader = WalReader::open(&path).unwrap();
        let records: Vec<WalRecord> = reader.filter_map(|r| r.ok()).collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].txn_id, 10);
        assert_eq!(records[0].record_type, 2);
        assert_eq!(records[0].payload, vec![0xAA, 0xBB]);
        assert_eq!(records[1].txn_id, 20);
        assert_eq!(records[1].record_type, 0);
        assert!(records[1].payload.is_empty());
        assert_eq!(records[2].txn_id, 30);
        assert_eq!(records[2].record_type, 1);
        assert_eq!(records[2].payload, b"abort-reason".to_vec());
    }

    /// Test 6c: WalReader on an empty file yields no records.
    #[test]
    fn wal_reader_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.log");
        std::fs::write(&path, b"").unwrap();
        let mut reader = WalReader::open(&path).unwrap();
        assert!(reader.next_record().unwrap().is_none());
    }
}
