//! The real Turso storage engine (`turso_core`) running as a guest.
//!
//! A `db` node opens a Turso database in WAL mode on its simulated disk and
//! serves a tiny text protocol over the simulated network. Turso does all of
//! its I/O through its `IO`/`File` traits; [`SimIO`] implements them on top of
//! `sim::os`, so every byte Turso writes lands in the node's volatile page
//! cache and only becomes durable when a sync completes:
//!
//! * reads, writes and truncates complete immediately (inside the call);
//! * `sync` calls `os::fsync_begin` and parks the completion with a
//!   tape-chosen latency. It completes (`os::fsync_finish` + callback) only on
//!   a later `IO::step` once virtual time has passed, so a crash can land
//!   between a commit's WAL write and its fsync;
//! * if Turso ever blocks inside one poll (spins on `step` without virtual
//!   time moving, as its blocking helpers do), parked syncs complete at once
//!   and the latency is charged as CPU time. The server avoids those paths
//!   (it opens with `Database::open_async`), so this does not happen in practice;
//! * time is `os::now()` / `os::wall_clock()`, randomness is `sdk::get_random()`,
//!   file locks are no-ops, `sleep`/`yield_now` never touch the host.
//!
//! Statements are driven by [`exec`]: whenever Turso reports pending IO it runs
//! the IO layer once and then awaits a short `os::sleep`, so faults, crashes
//! and other guest tasks interleave with the engine.
//!
//! App-level bugs (Turso itself is unchanged):
//! * `ack_early` - acknowledge an INSERT as soon as its commit's WAL fsync has
//!                 been issued, before it completed.
//! * `nosync`    - the IO layer's sync is a no-op that reports success (a VFS
//!                 that ignores fsync).
//! * `sync_off`  - run Turso with `PRAGMA synchronous = OFF`.
//! * `rm_wal`    - the startup script deletes the "stale" WAL before opening.

use crate::composer::{cmd, compose, Template};
use crate::faults::FaultConfig;
use crate::sdk::{self, Kind};
use crate::sim::{os, Addr, MachineSpec, MainFn, NodeId, Time, MS, US};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use turso_core::io::{FileId, FileSyncType};
use turso_core::storage::database::DatabaseFile;
use turso_core::{
    Buffer, Clock, Completion, ConnectOpts, Connection, Database, File, IOResult, LimboError, MonotonicInstant,
    OpenDbAsyncState, OpenFlags, OpenOptions, SqliteDialect, StepResult, WallClockInstant, IO,
};

pub static APP: crate::apps::App = crate::apps::App {
    name: "turso",
    about: "the real Turso storage engine (turso_core, WAL mode) behind a tiny SQL server",
    bugs: &[
        ("ack_early", "acknowledge an INSERT once its WAL fsync is issued, before it completes"),
        ("nosync", "the IO layer's sync is a no-op that reports success"),
        ("sync_off", "run Turso with PRAGMA synchronous = OFF"),
        ("rm_wal", "the startup script deletes the 'stale' WAL file before opening"),
    ],
    catalog,
    commands,
    machine: machine_for,
    max_time: MAX_TIME,
};

pub const MAX_TIME: Time = 3_000 * MS;
const DB: &str = "turso.db";
const WAL: &str = "turso.db-wal";

pub const DURABLE: &str = "Committed rows survive crashes";
pub const VALUES_OK: &str = "Rows read back hold the value the workload wrote";
pub const INTEGRITY_RECOVERY: &str = "Database passes integrity_check after recovery";
pub const INTEGRITY_LIVE: &str = "Database passes integrity_check while serving";
pub const OPENS: &str = "Database opens after a restart";
pub const SQL_OK: &str = "SQL statements never fail unexpectedly";
pub const S_RECOVERED: &str = "Server recovered after a crash";
pub const S_CHECKPOINT: &str = "A checkpoint ran";
pub const S_AUTO_CKPT: &str = "A commit triggered an automatic checkpoint";
pub const S_CKPT_TRUNCATE: &str = "A TRUNCATE checkpoint emptied the WAL";
pub const S_TORN_TAIL: &str = "Recovery found a torn or unsynced WAL tail";
pub const S_WAL_REPLAY: &str = "Recovery replayed committed frames from the WAL";
pub const S_RETRY_FOUND: &str = "A retried insert found its row already committed";
pub const S_GAVE_UP: &str = "A client gave up on an insert";
pub const R_EVENTUALLY: &str = "eventually_ check ran";
pub const R_FINALLY: &str = "finally_ check ran";

pub fn catalog() -> Vec<(&'static str, Kind)> {
    vec![
        (DURABLE, Kind::Always),
        (VALUES_OK, Kind::Always),
        (INTEGRITY_RECOVERY, Kind::AlwaysOrUnreachable),
        (INTEGRITY_LIVE, Kind::AlwaysOrUnreachable),
        (OPENS, Kind::AlwaysOrUnreachable),
        (SQL_OK, Kind::AlwaysOrUnreachable),
        (S_RECOVERED, Kind::Sometimes),
        (S_CHECKPOINT, Kind::Sometimes),
        (S_AUTO_CKPT, Kind::Sometimes),
        (S_CKPT_TRUNCATE, Kind::Sometimes),
        (S_TORN_TAIL, Kind::Sometimes),
        (S_WAL_REPLAY, Kind::Sometimes),
        (S_RETRY_FOUND, Kind::Sometimes),
        (S_GAVE_UP, Kind::Sometimes),
        (R_EVENTUALLY, Kind::Reachable),
        (R_FINALLY, Kind::Reachable),
    ]
}

#[derive(Clone, Default)]
pub struct Bugs {
    pub ack_early: bool,
    pub nosync: bool,
    pub sync_off: bool,
    pub rm_wal: bool,
}

impl Bugs {
    pub fn from(set: &BTreeSet<String>) -> Bugs {
        Bugs {
            ack_early: set.contains("ack_early"),
            nosync: set.contains("nosync"),
            sync_off: set.contains("sync_off"),
            rm_wal: set.contains("rm_wal"),
        }
    }
}

fn machine_for(bugs: &BTreeSet<String>, faults: FaultConfig, swarm: bool, buggify: bool) -> MachineSpec {
    machine(&Bugs::from(bugs), faults, swarm, buggify)
}

// ------------------------------------------------------------------ IO layer

/// Distinguishes IO instances in Turso's process-wide database registry, which
/// is keyed by file identity: every incarnation of every run gets a fresh
/// `Database` instead of a stale one left over from a crashed incarnation.
static NEXT_IO: AtomicU64 = AtomicU64::new(0);

struct ParkedSync {
    due: Time,
    token: os::SyncToken,
    c: Completion,
}

struct IoState {
    /// The node incarnation this IO belongs to; calls from anywhere else
    /// (destructors running after a crash, end of run) are refused.
    owner: (NodeId, u32),
    nosync: bool,
    parked: Vec<ParkedSync>,
    last_step: Time,
    spins: u32,
    /// Set once the server is serving: later writes to the database file are
    /// checkpoint backfill.
    serving: bool,
    db_writes: u64,
    blocking_syncs: u64,
}

struct Shared {
    st: RefCell<IoState>,
    id: u64,
}

impl Shared {
    fn check(&self) -> turso_core::Result<()> {
        if os::context() == Some(self.st.borrow().owner) {
            Ok(())
        } else {
            Err(LimboError::InternalError("simulated node crashed: IO from a dead incarnation".into()))
        }
    }
}

pub struct SimIO(Arc<Shared>);
struct SimFile {
    path: String,
    io: Arc<Shared>,
}

// The simulated machine is single-threaded: Turso's Send/Sync bounds are
// satisfied vacuously (MemorySimIO in Turso's own simulator does the same).
unsafe impl Send for SimIO {}
unsafe impl Sync for SimIO {}
unsafe impl Send for SimFile {}
unsafe impl Sync for SimFile {}

/// Parked syncs complete after this much virtual time.
fn sync_latency() -> Time {
    sdk::random_range(100 * US, 600 * US)
}
/// Spins of `IO::step` at one virtual instant before we decide Turso is
/// blocking inside a single poll (Database::open, a blocking checkpoint).
const BLOCKING_SPINS: u32 = 32;

impl SimIO {
    fn new(nosync: bool) -> SimIO {
        let owner = os::context().expect("SimIO created outside guest code");
        SimIO(Arc::new(Shared {
            st: RefCell::new(IoState {
                owner,
                nosync,
                parked: vec![],
                last_step: Time::MAX,
                spins: 0,
                serving: false,
                db_writes: 0,
                blocking_syncs: 0,
            }),
            id: NEXT_IO.fetch_add(1, Ordering::Relaxed),
        }))
    }
    fn has_parked_sync(&self) -> bool {
        !self.0.st.borrow().parked.is_empty()
    }
}

impl Clock for SimIO {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        let t = if os::context().is_some() { os::now() } else { 0 };
        MonotonicInstant::from_nanos(t as u128)
    }
    fn current_time_wall_clock(&self) -> WallClockInstant {
        // virtual wall clock (subject to clock_skip faults), anchored at 2025-01-01
        let w = if os::context().is_some() { os::wall_clock() } else { 0 } + 1_735_689_600_000_000_000;
        WallClockInstant { secs: w.div_euclid(1_000_000_000), micros: (w.rem_euclid(1_000_000_000) / 1000) as u32 }
    }
}

impl IO for SimIO {
    fn open_file(&self, path: &str, flags: OpenFlags, _direct: bool) -> turso_core::Result<Arc<dyn File>> {
        self.0.check()?;
        if !os::exists(path) {
            if !flags.contains(OpenFlags::Create) {
                return Err(LimboError::InternalError(format!("{path}: no such file")));
            }
            os::truncate(path, 0); // creates the file (metadata is durable)
        }
        Ok(Arc::new(SimFile { path: path.to_string(), io: self.0.clone() }))
    }

    fn remove_file(&self, path: &str) -> turso_core::Result<()> {
        self.0.check()?;
        // the simulated disk has no unlink: an empty file is the closest thing
        if os::exists(path) {
            os::truncate(path, 0);
        }
        Ok(())
    }

    fn step(&self) -> turso_core::Result<()> {
        self.0.check()?;
        let now = os::now();
        let due: Vec<ParkedSync> = {
            let mut st = self.0.st.borrow_mut();
            if st.last_step == now {
                st.spins += 1;
            } else {
                st.last_step = now;
                st.spins = 0;
            }
            if st.spins >= BLOCKING_SPINS && !st.parked.is_empty() {
                // Turso is blocking inside one poll: behave like a blocking
                // fsync(2) and charge its latency as CPU time.
                let wait = st.parked.iter().map(|p| p.due.saturating_sub(now)).max().unwrap_or(0);
                os::cpu(wait);
                st.blocking_syncs += st.parked.len() as u64;
                std::mem::take(&mut st.parked)
            } else {
                let (due, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut st.parked).into_iter().partition(|p| p.due <= now);
                st.parked = wait;
                due
            }
        };
        // complete outside the borrow: callbacks may issue new IO
        for p in due {
            os::fsync_finish(p.token);
            p.c.complete(0);
        }
        Ok(())
    }

    fn generate_random_number(&self) -> i64 {
        if os::context().is_some() { sdk::get_random() as i64 } else { 0 }
    }

    fn fill_bytes(&self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let r = self.generate_random_number().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
    }

    fn yield_now(&self) {}

    fn sleep(&self, d: std::time::Duration) {
        if os::context().is_some() {
            os::cpu(d.as_nanos() as Time);
        }
    }

    fn file_id(&self, path: &str) -> turso_core::Result<FileId> {
        Ok(FileId::from_path_hash(&format!("{path}@detvisor-io-{}", self.0.id)))
    }
}

impl File for SimFile {
    fn lock_file(&self, _exclusive: bool) -> turso_core::Result<()> {
        Ok(())
    }
    fn unlock_file(&self) -> turso_core::Result<()> {
        Ok(())
    }

    fn pread(&self, pos: u64, c: Completion) -> turso_core::Result<Completion> {
        self.io.check()?;
        let n = {
            let buf = c.as_read().buf();
            let data = os::pread(&self.path, pos, buf.len());
            let dst = buf.as_mut_slice();
            dst[..data.len()].copy_from_slice(&data);
            dst[data.len()..].fill(0);
            data.len()
        };
        c.complete(n as i32);
        Ok(c)
    }

    fn pwrite(&self, pos: u64, buffer: Arc<Buffer>, c: Completion) -> turso_core::Result<Completion> {
        self.io.check()?;
        let data = buffer.as_slice();
        os::pwrite(&self.path, pos, data);
        if self.path == DB {
            let mut st = self.io.st.borrow_mut();
            if st.serving {
                st.db_writes += 1;
            }
        }
        c.complete(data.len() as i32);
        Ok(c)
    }

    fn pwritev(&self, pos: u64, buffers: Vec<Arc<Buffer>>, c: Completion) -> turso_core::Result<Completion> {
        self.io.check()?;
        // one page-cache write per buffer, so a crash can keep a prefix of them
        let mut off = pos;
        for b in &buffers {
            os::pwrite(&self.path, off, b.as_slice());
            off += b.len() as u64;
        }
        if self.path == DB {
            let mut st = self.io.st.borrow_mut();
            if st.serving {
                st.db_writes += buffers.len() as u64;
            }
        }
        c.complete((off - pos) as i32);
        Ok(c)
    }

    fn sync(&self, c: Completion, _sync_type: FileSyncType) -> turso_core::Result<Completion> {
        self.io.check()?;
        if self.io.st.borrow().nosync {
            // BUG(nosync): report success without making anything durable.
            c.complete(0);
            return Ok(c);
        }
        let token = os::fsync_begin(&self.path);
        let due = os::now() + sync_latency();
        self.io.st.borrow_mut().parked.push(ParkedSync { due, token, c: c.clone() });
        Ok(c)
    }

    fn size(&self) -> turso_core::Result<u64> {
        self.io.check()?;
        Ok(os::file_size(&self.path))
    }

    fn truncate(&self, len: u64, c: Completion) -> turso_core::Result<Completion> {
        self.io.check()?;
        os::truncate(&self.path, len);
        c.complete(0);
        Ok(c)
    }
}

// ------------------------------------------------------------------ driving statements

/// Run one statement to completion. Whenever Turso reports pending IO, run the
/// IO layer once, call `on_io`, and sleep a little so that time passes and
/// other tasks, faults and crashes interleave.
async fn exec(conn: &Arc<Connection>, io: &SimIO, sql: &str, mut on_io: impl FnMut(&SimIO)) -> Result<Vec<Vec<String>>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = vec![];
    loop {
        match stmt.step().map_err(|e| e.to_string())? {
            StepResult::Row => {
                let r = stmt.row().expect("row");
                rows.push((0..r.len()).map(|i| r.get_value(i).to_string()).collect());
            }
            StepResult::Done => return Ok(rows),
            StepResult::IO | StepResult::Yield => {
                io.step().map_err(|e| e.to_string())?;
                on_io(io);
                os::sleep(sdk::random_range(20 * US, 150 * US)).await;
            }
            StepResult::Sleep { duration } => os::sleep(duration.as_nanos() as Time).await,
            StepResult::Busy => return Err("SQLITE_BUSY".into()),
            StepResult::Interrupt => return Err("interrupted".into()),
        }
    }
}

async fn exec_ok(conn: &Arc<Connection>, io: &SimIO, sql: &str) -> Option<Vec<Vec<String>>> {
    match exec(conn, io, sql, |_| {}).await {
        Ok(r) => Some(r),
        Err(e) => {
            sdk::assert_always_or_unreachable(false, SQL_OK, &format!("{sql}: {e}"));
            None
        }
    }
}

// ------------------------------------------------------------------ WAL inspection

/// What the WAL file looks like before Turso recovers it (SQLite WAL format).
#[derive(Debug, Default)]
struct WalScan {
    /// Frames up to and including the last valid commit frame.
    committed: usize,
    /// Frames after that which carry the current salt: written but never
    /// committed durably, or torn (bad checksum).
    tail_frames: usize,
    /// Bytes of a trailing partial frame.
    partial: usize,
}

fn scan_wal(w: &[u8]) -> Option<WalScan> {
    if w.len() < 32 {
        return None;
    }
    let be = |b: &[u8], i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
    let magic = be(w, 0);
    if magic & !1 != 0x377f0682 {
        return None;
    }
    let big = magic & 1 == 1;
    let cksum = |data: &[u8], (mut s0, mut s1): (u32, u32)| {
        for c in data.chunks_exact(8) {
            let (a, b) = if big { (be(c, 0), be(c, 4)) } else { (u32::from_le_bytes(c[0..4].try_into().unwrap()), u32::from_le_bytes(c[4..8].try_into().unwrap())) };
            s0 = s0.wrapping_add(a.wrapping_add(s1));
            s1 = s1.wrapping_add(b.wrapping_add(s0));
        }
        (s0, s1)
    };
    let page = be(w, 8) as usize;
    let (salt1, salt2) = (be(w, 16), be(w, 20));
    let mut sum = cksum(&w[..24], (0, 0));
    if sum != (be(w, 24), be(w, 28)) || page < 512 {
        return None;
    }
    let frame = 24 + page;
    let mut s = WalScan::default();
    let (mut pos, mut i, mut valid) = (32, 0, true);
    while pos + frame <= w.len() {
        let fh = &w[pos..pos + 24];
        i += 1;
        if be(fh, 8) != salt1 || be(fh, 12) != salt2 {
            break; // stale frame from before a WAL restart
        }
        if valid {
            let next = cksum(&w[pos + 24..pos + frame], cksum(&fh[..8], sum));
            if next == (be(fh, 16), be(fh, 20)) {
                sum = next;
                if be(fh, 4) > 0 {
                    s.committed = i;
                }
            } else {
                valid = false;
            }
        }
        pos += frame;
    }
    s.tail_frames = i - s.committed;
    if pos < w.len() && (w.len() - 32) % frame != 0 {
        s.partial = w.len() - pos;
    }
    Some(s)
}

// ------------------------------------------------------------------ server

async fn server(bugs: Rc<Bugs>) {
    let restarted = os::exists(DB);
    if bugs.rm_wal && os::exists(WAL) {
        // BUG(rm_wal): "a WAL left over from a crash is stale, start clean"
        os::log("startup: removing stale WAL");
        os::truncate(WAL, 0);
        os::fsync(WAL).await;
    }
    if restarted {
        let wal = os::read_bytes(WAL);
        match scan_wal(&wal) {
            Some(s) => {
                os::log(format!(
                    "recovery: WAL has {} bytes: {} committed frame(s), {} uncommitted/torn tail frame(s), {} partial bytes",
                    wal.len(),
                    s.committed,
                    s.tail_frames,
                    s.partial
                ));
                sdk::assert_sometimes(s.committed > 0, S_WAL_REPLAY, "");
                sdk::assert_sometimes(s.tail_frames > 0 || s.partial > 0, S_TORN_TAIL, &format!("{s:?}"));
            }
            None => os::log(format!("recovery: WAL has {} bytes, no valid header", wal.len())),
        }
    }

    let io = Arc::new(SimIO::new(bugs.nosync));
    let conn = match open(&io).await.and_then(|db| db.connect(ConnectOpts::new()).map_err(|e| e.to_string())) {
        Ok(c) => c,
        Err(e) => {
            sdk::assert_always_or_unreachable(false, OPENS, &e.to_string());
            os::log(format!("open failed: {e}"));
            return;
        }
    };
    let Some(mode) = exec_ok(&conn, &io, "PRAGMA journal_mode = wal").await else { return };
    // checkpoint often so runs see checkpoints (and crashes during them)
    exec_ok(&conn, &io, "PRAGMA wal_autocheckpoint = 16").await;
    if bugs.sync_off {
        exec_ok(&conn, &io, "PRAGMA synchronous = OFF").await;
    }
    // `pad` (up to ~1.5 pages) and the index make commits span several pages
    // and frames: overflow pages, b-tree splits, multi-frame WAL commits.
    exec_ok(&conn, &io, "CREATE TABLE IF NOT EXISTS kv (id INTEGER PRIMARY KEY, val TEXT NOT NULL, pad BLOB)").await;
    exec_ok(&conn, &io, "CREATE INDEX IF NOT EXISTS kv_val ON kv (val)").await;
    if restarted {
        let res = integrity_check(&conn, &io).await;
        sdk::assert_always_or_unreachable(res == "ok", INTEGRITY_RECOVERY, &res);
        sdk::assert_sometimes(true, S_RECOVERED, "");
    }
    let n = exec_ok(&conn, &io, "SELECT count(*) FROM kv").await.map(|r| flat(&r)).unwrap_or_default();
    os::log(format!("turso open: journal_mode={} rows={n}", flat(&mode)));
    sdk::export_state("db/rows", n);
    io.0.st.borrow_mut().serving = true;

    let sock = os::bind(80);
    sdk::setup_complete();
    let mut seen_writes = 0;
    loop {
        // one request at a time on one connection (like a single-writer server)
        let p = sock.recv().await;
        if let Some(r) = handle(&p.body, &conn, &io, &bugs, &sock, p.from).await {
            sock.send(p.from, r);
        }
        let (w, blocking) = {
            let st = io.0.st.borrow();
            (st.db_writes, st.blocking_syncs)
        };
        if w > seen_writes {
            sdk::assert_sometimes(true, S_CHECKPOINT, "");
            sdk::assert_sometimes(p.body.starts_with("INSERT"), S_AUTO_CKPT, "");
            os::log(format!("checkpoint backfilled {} page write(s) into {DB}", w - seen_writes));
            seen_writes = w;
        }
        sdk::export_state("db/io", format!("{w} checkpoint page writes, {blocking} syncs completed by a blocking wait"));
    }
}

/// Open (and recover) the database with Turso's non-blocking open, driven
/// like a statement so that time passes and a crash can land mid-recovery.
async fn open(io: &Arc<SimIO>) -> Result<Arc<Database>, String> {
    let file = io.open_file(DB, OpenFlags::default(), true).map_err(|e| e.to_string())?;
    let opts = OpenOptions::new(Arc::new(SqliteDialect)).storage(Arc::new(DatabaseFile::new(file)));
    let mut state = OpenDbAsyncState::new();
    loop {
        match Database::open_async(&mut state, io.clone() as Arc<dyn IO>, DB, &opts).map_err(|e| e.to_string())? {
            IOResult::Done(db) => return Ok(db),
            IOResult::IO(_) => {
                io.step().map_err(|e| e.to_string())?;
                os::sleep(sdk::random_range(20 * US, 150 * US)).await;
            }
        }
    }
}

/// "ok", the problems found, or the error that stopped the check (a short
/// read of a page counts as corruption, not as an unexpected SQL failure).
async fn integrity_check(conn: &Arc<Connection>, io: &SimIO) -> String {
    match exec(conn, io, "PRAGMA integrity_check", |_| {}).await {
        Ok(r) => flat(&r),
        Err(e) => format!("error:_{e}").replace(' ', "_"),
    }
}

fn flat(rows: &[Vec<String>]) -> String {
    rows.iter().map(|r| r.join(",")).collect::<Vec<_>>().join(";").replace(' ', "_")
}

async fn handle(body: &str, conn: &Arc<Connection>, io: &SimIO, bugs: &Bugs, sock: &os::Socket, from: Addr) -> Option<String> {
    let p: Vec<&str> = body.split(' ').collect();
    match p[0] {
        "PING" => Some(format!("PONG {}", p[1])),
        "INSERT" => {
            let (id, val, pad) = (p[1].parse::<i64>().ok()?, p[2], p[3].parse::<u32>().ok()?);
            let found = exec_ok(conn, io, &format!("SELECT val FROM kv WHERE id = {id}")).await?;
            if !found.is_empty() {
                // a retry of an insert that committed (its reply was lost)
                sdk::assert_sometimes(true, S_RETRY_FOUND, &id.to_string());
                return Some(format!("OK {id} dup"));
            }
            let mut acked = false;
            let reply = format!("OK {id}");
            let r = exec(conn, io, &format!("INSERT INTO kv VALUES ({id}, '{val}', zeroblob({pad}))"), |io| {
                if bugs.ack_early && !acked && io.has_parked_sync() {
                    // BUG(ack_early): the commit's fsync is in flight; reply now.
                    acked = true;
                    sock.send(from, reply.clone());
                }
            })
            .await;
            match r {
                Ok(_) if acked => None,
                Ok(_) => Some(reply),
                Err(e) => {
                    sdk::assert_always_or_unreachable(false, SQL_OK, &format!("insert {id}: {e}"));
                    Some(format!("ERR {id}"))
                }
            }
        }
        "READ" => {
            let rows = exec_ok(conn, io, "SELECT id, val, length(pad) FROM kv ORDER BY id").await?;
            sdk::export_state("db/rows", rows.len().to_string());
            let kv: Vec<String> = rows.iter().map(|r| format!("{}:{}/{}", r[0], r[1], r[2])).collect();
            Some(format!("ROWS {} {}", p[1], kv.join(",")))
        }
        "CHECK" => {
            Some(format!("CHECKED {} {}", p[1], integrity_check(conn, io).await))
        }
        "CKPT" => {
            let r = exec_ok(conn, io, "PRAGMA wal_checkpoint(TRUNCATE)").await?;
            if os::file_size(WAL) == 0 {
                sdk::assert_sometimes(true, S_CKPT_TRUNCATE, "");
            }
            Some(format!("CKPTED {} {}", p[1], flat(&r)))
        }
        _ => None,
    }
}

// ------------------------------------------------------------------ workload

#[derive(Default)]
struct Ledger {
    next: i64,
    /// every insert ever sent: id -> "value/pad length"
    sent: BTreeMap<i64, String>,
    /// inserts the server acknowledged
    acked: BTreeSet<i64>,
}

fn db_addr() -> Addr {
    Addr { node: os::node_id("db"), port: 80 }
}

/// Send `msg` until a reply for request `id` arrives.
async fn rpc(sock: &os::Socket, id: &str, msg: &str, attempts: u32) -> Option<String> {
    for _ in 0..attempts {
        sock.send(db_addr(), msg);
        let deadline = os::now() + 20 * MS;
        while os::now() < deadline {
            match sock.recv_timeout(deadline - os::now()).await {
                Some(p) if p.body.split(' ').nth(1) == Some(id) => return Some(p.body),
                Some(_) => continue, // a late reply to an earlier request
                None => break,
            }
        }
    }
    None
}

async fn inserts(ledger: Rc<RefCell<Ledger>>, n: u64) {
    let sock = os::bind_ephemeral();
    for _ in 0..n {
        let (id, val) = {
            let mut l = ledger.borrow_mut();
            l.next += 1;
            let (id, val, pad) = (l.next, format!("v{}", sdk::random_range(0, 1_000_000)), sdk::random_range(0, 6000));
            l.sent.insert(id, format!("{val}/{pad}"));
            (id, format!("{val} {pad}"))
        };
        match rpc(&sock, &id.to_string(), &format!("INSERT {id} {val}"), 6).await {
            Some(r) if r.starts_with("OK") => {
                ledger.borrow_mut().acked.insert(id);
            }
            _ => {
                sdk::assert_sometimes(true, S_GAVE_UP, &id.to_string());
                sdk::stop_faults(20 * MS);
            }
        }
        let acked = ledger.borrow().acked.len();
        sdk::export_state("workload/acked", format!("{acked} acknowledged inserts"));
        os::sleep(sdk::random_range(0, 3 * MS)).await;
    }
}

/// Read every row; check that every acknowledged insert is there with its
/// value and that nothing else appeared. Then run integrity_check.
async fn check(ledger: &Rc<RefCell<Ledger>>, attempts: u32) -> bool {
    let acked = ledger.borrow().acked.clone();
    let sock = os::bind_ephemeral();
    let rid = format!("r{}", sdk::get_random() % 1_000_000);
    let Some(state) = rpc(&sock, &rid, &format!("READ {rid}"), attempts).await else { return false };
    let rows: BTreeMap<i64, String> = state
        .split(' ')
        .nth(2)
        .unwrap_or("")
        .split(',')
        .filter_map(|kv| kv.split_once(':'))
        .filter_map(|(k, v)| Some((k.parse().ok()?, v.to_string())))
        .collect();
    let missing: Vec<&i64> = acked.iter().filter(|id| !rows.contains_key(id)).collect();
    sdk::assert_always(missing.is_empty(), DURABLE, &format!("acknowledged but missing after recovery: {missing:?}"));
    let wrong: Vec<String> = {
        let l = ledger.borrow();
        rows.iter().filter(|(k, v)| l.sent.get(k) != Some(v)).map(|(k, v)| format!("{k}:{v}")).collect()
    };
    sdk::assert_always(wrong.is_empty(), VALUES_OK, &format!("rows nobody wrote: {wrong:?}"));
    let cid = format!("c{}", sdk::get_random() % 1_000_000);
    if let Some(r) = rpc(&sock, &cid, &format!("CHECK {cid}"), attempts).await {
        let res = r.split(' ').nth(2).unwrap_or("");
        sdk::assert_always_or_unreachable(res == "ok", INTEGRITY_LIVE, res);
    }
    true
}

fn template(ledger: Rc<RefCell<Ledger>>) -> Template {
    let first: MainFn = Rc::new(|| {
        Box::pin(async {
            let sock = os::bind_ephemeral();
            while rpc(&sock, "p0", "PING p0", 1).await.is_none() {}
        })
    });
    let l = ledger.clone();
    let par: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move { inserts(l, sdk::random_range(2, 8)).await })
    });
    let l = ledger.clone();
    let single: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move { inserts(l, 60).await })
    });
    let l = ledger.clone();
    let audit: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move {
            check(&l, 2).await;
        })
    });
    let ckpt: MainFn = Rc::new(|| {
        Box::pin(async {
            if sdk::random_choice(3) != 0 {
                return; // manual checkpoints are occasional
            }
            let sock = os::bind_ephemeral();
            let id = format!("k{}", sdk::get_random() % 1_000_000);
            rpc(&sock, &id, &format!("CKPT {id}"), 2).await;
        })
    });
    let l = ledger.clone();
    let eventually: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move {
            while !check(&l, 4).await {}
            sdk::assert_reachable(R_EVENTUALLY, "");
        })
    });
    let l = ledger;
    let finally: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move {
            while !check(&l, 4).await {}
            sdk::assert_reachable(R_FINALLY, "");
        })
    });
    Template {
        commands: vec![
            cmd("first_wait_for_db", first),
            cmd("parallel_driver_inserts", par),
            cmd("singleton_driver_long_insert_run", single),
            cmd("anytime_read_and_check", audit),
            cmd("anytime_checkpoint_truncate", ckpt),
            cmd("eventually_recovered_durability", eventually),
            cmd("finally_durability", finally),
        ],
        driver_time: 400 * MS,
        max_parallel: 4,
    }
}

/// A faultable `db` node running Turso, and a `workload` node hosting the
/// Test Composer.
pub fn machine(bugs: &Bugs, faults: FaultConfig, swarm: bool, buggify: bool) -> MachineSpec {
    let bugs = Rc::new(bugs.clone());
    let main: MainFn = Rc::new(move || Box::pin(server(bugs.clone())));
    let tmpl = Rc::new(template(Rc::new(RefCell::new(Ledger::default()))));
    let composer: MainFn = Rc::new(move || Box::pin(compose(tmpl.clone())));
    MachineSpec {
        nodes: vec![("db".into(), true, Some(main)), ("workload".into(), false, None)],
        catalog: catalog(),
        composer: ("workload".into(), composer),
        faults,
        swarm,
        buggify,
    }
}

pub fn commands() -> Vec<(&'static str, crate::composer::CmdKind)> {
    template(Rc::new(RefCell::new(Ledger::default()))).commands.into_iter().map(|c| (c.name, c.kind)).collect()
}
