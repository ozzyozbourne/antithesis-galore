//! The system under test: a tiny durable bank server plus its test template.
//!
//! The server keeps balances in memory, writes every transfer to a write-ahead
//! log, periodically compacts the log into a snapshot, and recovers from disk
//! after a crash. Four realistic bugs can be switched on, each one found by a
//! different technique:
//!
//! * `race`    - read-modify-write across an await: found by the scheduler
//!               exploring thread interleavings.
//! * `ack`     - acknowledges a transfer before fsync: found by crash faults
//!               plus the durability check in anytime_/eventually_/finally_.
//! * `dup`     - no deduplication of retried requests: found by packet loss /
//!               duplication plus client retries.
//! * `compact` - truncates the log before the snapshot is durable: needs a crash
//!               inside a ~1ms window during compaction. Rare under random
//!               testing; guided exploration branches from the "Log compaction
//!               started" sometimes-assertion.
//! * `torn`    - recovery trusts the log without checking checksums: a crash
//!               that tears the last append crashes recovery. Found by the
//!               torn-write disk fault.
//! * `leak`    - not a correctness bug: uses std HashMap iteration order, a
//!               source of nondeterminism the hypervisor cannot control.

use crate::composer::{cmd, compose, Template};
use crate::faults::FaultConfig;
use crate::sdk::{self, Kind};
use crate::sim::{os, Addr, MachineSpec, MainFn, Time, MS};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

pub static APP: crate::apps::App = crate::apps::App {
    name: "bank",
    about: "single-node durable bank: WAL, group commit, compaction, crash recovery",
    bugs: &[
        ("race", "read-modify-write across an await (lost update)"),
        ("ack", "acknowledge before the WAL is fsynced"),
        ("dup", "no deduplication of retried requests"),
        ("compact", "truncate the log before the snapshot is durable"),
        ("torn", "recovery trusts log lines without checking checksums"),
        ("leak", "std HashMap iteration order (nondeterminism leak)"),
    ],
    catalog,
    commands,
    machine: machine_for,
    max_time: MAX_TIME,
};

fn machine_for(bugs: &BTreeSet<String>, faults: FaultConfig, swarm: bool, buggify: bool) -> MachineSpec {
    machine(&Bugs::from(bugs), faults, swarm, buggify)
}

pub const CONSERVED: &str = "Money is conserved";
pub const AT_MOST_ONCE: &str = "Each transfer is applied at most once";
pub const DURABLE: &str = "Acknowledged transfers survive crashes";
pub const RECOVERY_OK: &str = "Recovery finds a snapshot that covers the truncated log";
pub const SNAPSHOT_PARSE: &str = "Snapshot file is never corrupt";
pub const S_RETRY_DEDUP: &str = "Server deduplicated a retried transfer";
pub const S_RECOVERED: &str = "Server recovered state after a restart";
pub const S_COMPACT_START: &str = "Log compaction started";
pub const S_COMPACT_DONE: &str = "Log compaction finished";
pub const S_CONCURRENT: &str = "Two transfers were in flight at once";
pub const S_GAVE_UP: &str = "A client gave up on a transfer";
pub const R_EVENTUALLY: &str = "eventually_ check ran";
pub const R_FINALLY: &str = "finally_ check ran";
pub const U_BAD_WAL: &str = "WAL is never corrupted before its tail";
pub const S_TORN_TAIL: &str = "Recovery discarded a torn log tail";

pub fn catalog() -> Vec<(&'static str, Kind)> {
    vec![
        (CONSERVED, Kind::Always),
        (AT_MOST_ONCE, Kind::Always),
        (DURABLE, Kind::Always),
        (RECOVERY_OK, Kind::AlwaysOrUnreachable),
        (SNAPSHOT_PARSE, Kind::AlwaysOrUnreachable),
        (S_RETRY_DEDUP, Kind::Sometimes),
        (S_RECOVERED, Kind::Sometimes),
        (S_COMPACT_START, Kind::Sometimes),
        (S_COMPACT_DONE, Kind::Sometimes),
        (S_CONCURRENT, Kind::Sometimes),
        (S_GAVE_UP, Kind::Sometimes),
        (R_EVENTUALLY, Kind::Reachable),
        (R_FINALLY, Kind::Reachable),
        (U_BAD_WAL, Kind::Unreachable),
        (S_TORN_TAIL, Kind::Sometimes),
    ]
}

const ACCOUNTS: [&str; 4] = ["A", "B", "C", "D"];
const TOTAL: i64 = 400;
/// The log must grow this long before compaction: a deep, late state.
const COMPACT_AT: usize = 120;

#[derive(Clone, Default)]
pub struct Bugs {
    pub race: bool,
    pub ack: bool,
    pub dup: bool,
    pub compact: bool,
    pub torn: bool,
    pub leak: bool,
}

impl Bugs {
    pub fn from(set: &BTreeSet<String>) -> Bugs {
        Bugs {
            race: set.contains("race"),
            ack: set.contains("ack"),
            dup: set.contains("dup"),
            compact: set.contains("compact"),
            torn: set.contains("torn"),
            leak: set.contains("leak"),
        }
    }
}

// ------------------------------------------------------------------ server

#[derive(Default)]
struct Server {
    balances: BTreeMap<String, i64>,
    applied: BTreeSet<String>,
    inflight: BTreeSet<String>,
    seq: u64,
    gen: u64,
}

impl Server {
    fn total(&self) -> i64 {
        self.balances.values().sum()
    }
    fn export(&self) {
        let b: Vec<String> = self.balances.iter().map(|(k, v)| format!("{k}={v}")).collect();
        sdk::export_state("bank/balances", format!("{} (total {})", b.join(" "), self.total()));
        sdk::export_state("bank/applied", format!("{} transfers, snapshot gen {}", self.applied.len(), self.gen));
    }
    fn snapshot_lines(&self) -> Vec<String> {
        let mut v = vec![format!("gen {}", self.gen)];
        v.extend(self.balances.iter().map(|(k, b)| format!("bal {k} {b}")));
        v.extend(self.applied.iter().map(|r| format!("applied {r}")));
        v.push("end".into());
        v.into_iter().map(seal).collect()
    }
}

fn apply_tx(s: &mut Server, req: &str, from: &str, to: &str, amt: i64) {
    *s.balances.get_mut(from).unwrap() -= amt;
    *s.balances.get_mut(to).unwrap() += amt;
    s.applied.insert(req.to_string());
}

pub(crate) fn crc(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}
/// Every line on disk carries a checksum so torn or rotten lines are detectable.
pub(crate) fn seal(l: String) -> String {
    let c = crc(&l);
    format!("{l} #{c:08x}")
}
pub(crate) fn unseal(l: &str) -> Option<&str> {
    let (body, c) = l.rsplit_once(" #")?;
    (u32::from_str_radix(c, 16).ok()? == crc(body)).then_some(body)
}
/// BUG(torn): strip the checksum without checking it.
fn strip(l: &str) -> &str {
    l.rsplit_once(" #").map(|x| x.0).unwrap_or(l)
}

async fn recover(bugs: &Bugs) -> Server {
    let mut s = Server::default();
    let snap = os::read("snapshot");
    let wal = os::read("wal");
    if snap.is_empty() && wal.is_empty() {
        for a in ACCOUNTS {
            s.balances.insert(a.into(), TOTAL / 4);
        }
        os::replace("snapshot", s.snapshot_lines());
        os::fsync("snapshot").await;
        os::replace("wal", vec![seal("base 0".into())]);
        os::fsync("wal").await;
        os::log("initialised fresh database");
        return s;
    }
    let snap: Vec<&str> = if bugs.torn {
        snap.iter().map(|l| strip(l)).collect()
    } else {
        let checked: Option<Vec<&str>> = snap.iter().map(|l| unseal(l)).collect();
        let ok = checked.as_ref().is_some_and(|v| v.last() == Some(&"end"));
        sdk::assert_always_or_unreachable(ok, SNAPSHOT_PARSE, "snapshot checksum or end marker missing");
        checked.unwrap_or_default()
    };
    for l in &snap {
        let p: Vec<&str> = l.split(' ').collect();
        match p[0] {
            "gen" => s.gen = p[1].parse().unwrap(),
            "bal" => {
                s.balances.insert(p[1].into(), p[2].parse().unwrap());
            }
            "applied" => {
                s.applied.insert(p[1].into());
            }
            _ => {}
        }
    }
    let mut replayed = 0;
    let mut valid: Vec<String> = vec![];
    for (i, raw) in wal.iter().enumerate() {
        let body = if bugs.torn {
            strip(raw)
        } else {
            match unseal(raw) {
                Some(b) => b,
                None if i + 1 == wal.len() => {
                    // a crash tore the last append: it was never acknowledged
                    sdk::assert_sometimes(true, S_TORN_TAIL, raw);
                    os::log(format!("discarding torn WAL tail {raw:?}"));
                    os::replace("wal", valid.clone());
                    os::fsync("wal").await;
                    break;
                }
                None => {
                    sdk::assert_unreachable(U_BAD_WAL, raw);
                    break;
                }
            }
        };
        valid.push(raw.clone());
        let p: Vec<&str> = body.split(' ').collect();
        match p[0] {
            "base" => {
                let base: u64 = p[1].parse().unwrap();
                sdk::assert_always_or_unreachable(
                    s.gen >= base,
                    RECOVERY_OK,
                    &format!("log was truncated up to snapshot gen {base}, but the durable snapshot is gen {}", s.gen),
                );
            }
            "tx" => {
                let (req, from, to, amt) = (p[2], p[3], p[4], p[5].parse::<i64>().unwrap());
                if !s.applied.contains(req) {
                    apply_tx(&mut s, req, from, to, amt);
                    replayed += 1;
                }
            }
            _ => sdk::assert_unreachable(U_BAD_WAL, raw),
        }
        s.seq += 1;
    }
    sdk::assert_sometimes(true, S_RECOVERED, "");
    os::log(format!("recovered: snapshot gen {}, {} applied, replayed {replayed} from WAL", s.gen, s.applied.len()));
    s
}

async fn server(bugs: Rc<Bugs>) {
    let s = Rc::new(RefCell::new(recover(&bugs).await));
    s.borrow().export();
    os::spawn("compactor", compactor(s.clone(), bugs.clone()));
    let sock = os::bind(80);
    sdk::setup_complete();
    loop {
        let p = sock.recv().await;
        let (s, bugs) = (s.clone(), bugs.clone());
        os::spawn(format!("req {}", p.body.split(' ').nth(1).unwrap_or("?")), async move {
            let reply = handle(&p.body, &s, &bugs).await;
            if let Some(r) = reply {
                sock.send(p.from, r);
            }
        });
    }
}

async fn handle(body: &str, st: &Rc<RefCell<Server>>, bugs: &Bugs) -> Option<String> {
    let p: Vec<&str> = body.split(' ').collect();
    match p[0] {
        "PING" => Some(format!("PONG {}", p[1])),
        "READ" => {
            let s = st.borrow();
            let bal = if bugs.leak {
                // BUG(leak): HashMap iteration order is randomised per process.
                let h: HashMap<&String, &i64> = s.balances.iter().collect();
                h.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
            } else {
                s.balances.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
            };
            let applied: Vec<&str> = s.applied.iter().map(|x| x.as_str()).collect();
            Some(format!("STATE {} total={} bal={} applied={}", p[1], s.total(), bal, applied.join(",")))
        }
        "TX" => {
            let (req, from, to, amt) = (p[1], p[2], p[3], p[4].parse::<i64>().unwrap());
            {
                let mut s = st.borrow_mut();
                if !bugs.dup {
                    if s.applied.contains(req) {
                        sdk::assert_sometimes(true, S_RETRY_DEDUP, req);
                        return Some(format!("OK {req} dup"));
                    }
                    if s.inflight.contains(req) {
                        return Some(format!("BUSY {req}"));
                    }
                }
                sdk::assert_sometimes(!s.inflight.is_empty(), S_CONCURRENT, "");
                s.inflight.insert(req.to_string());
                s.seq += 1;
                let seq = s.seq;
                os::append("wal", seal(format!("tx {seq} {req} {from} {to} {amt}")));
            }
            if bugs.ack {
                // BUG(ack): apply and acknowledge before the log is durable.
                commit(st, req, from, to, amt, None);
                let s2 = st.clone();
                let r = req.to_string();
                os::spawn("lazy-fsync", async move {
                    os::fsync("wal").await;
                    let _ = (s2, r);
                });
                return Some(format!("OK {req}"));
            }
            // Read balances now, write them after the fsync (BUG(race) only).
            let read = bugs.race.then(|| {
                let s = st.borrow();
                (s.balances[from], s.balances[to])
            });
            // group commit: wait for the next flush so concurrent appends share one fsync
            os::sleep(sdk::random_range(0, 2 * MS)).await;
            if sdk::buggify("slow fsync") {
                os::sleep(5 * MS).await;
            }
            os::fsync("wal").await;
            commit(st, req, from, to, amt, read);
            Some(format!("OK {req}"))
        }
        _ => None,
    }
}

fn commit(st: &Rc<RefCell<Server>>, req: &str, from: &str, to: &str, amt: i64, read: Option<(i64, i64)>) {
    let mut s = st.borrow_mut();
    sdk::assert_always(!s.applied.contains(req), AT_MOST_ONCE, &format!("{req} applied twice"));
    match read {
        Some((fb, tb)) => {
            // BUG(race): lost update if another transfer committed in between.
            s.balances.insert(from.into(), fb - amt);
            s.balances.insert(to.into(), tb + amt);
            s.applied.insert(req.to_string());
        }
        None => apply_tx(&mut s, req, from, to, amt),
    }
    s.inflight.remove(req);
    let total = s.total();
    sdk::assert_always(total == TOTAL, CONSERVED, &format!("total is {total} after {req}"));
    s.export();
}

/// Background log compaction: snapshot the state, then truncate the log.
async fn compactor(st: Rc<RefCell<Server>>, bugs: Rc<Bugs>) {
    loop {
        os::sleep(25 * MS).await;
        if os::read("wal").len() < COMPACT_AT {
            continue;
        }
        sdk::assert_sometimes(true, S_COMPACT_START, "");
        sdk::send_event("compaction", "starting");
        let applied = {
            let mut s = st.borrow_mut();
            s.gen += 1;
            os::replace("snapshot", s.snapshot_lines());
            s.applied.clone()
        };
        let gen = st.borrow().gen;
        let truncate = || {
            let mut keep = vec![seal(format!("base {gen}"))];
            keep.extend(os::read("wal").into_iter().filter(|l| {
                let p: Vec<&str> = strip(l).split(' ').collect();
                p[0] == "tx" && !applied.contains(p[2])
            }));
            os::replace("wal", keep);
        };
        if bugs.compact {
            // BUG(compact): truncate the log first, sync the snapshot later.
            truncate();
            os::fsync("wal").await;
            os::sleep(3 * MS).await; // "the background flusher will get to it"
            os::fsync("snapshot").await;
        } else {
            os::fsync("snapshot").await;
            truncate();
            os::fsync("wal").await;
        }
        sdk::assert_sometimes(true, S_COMPACT_DONE, "");
        os::log(format!("compaction to gen {gen} done"));
    }
}

// ------------------------------------------------------------------ workload

#[derive(Default)]
struct Ledger {
    next: u64,
    acked: BTreeMap<String, String>,
}

fn bank_addr() -> Addr {
    Addr { node: os::node_id("bank"), port: 80 }
}

/// Send `msg` until a reply starting with `want` for request `id` arrives.
async fn rpc(sock: &os::Socket, id: &str, msg: &str, attempts: u32) -> Option<String> {
    for _ in 0..attempts {
        sock.send(bank_addr(), msg);
        let deadline = os::now() + 15 * MS;
        while os::now() < deadline {
            match sock.recv_timeout(deadline - os::now()).await {
                Some(p) if p.body.split(' ').nth(1) == Some(id) => {
                    if p.body.starts_with("BUSY") {
                        os::sleep(2 * MS).await;
                        break;
                    }
                    return Some(p.body);
                }
                Some(_) => continue, // a late reply to an earlier request
                None => break,
            }
        }
    }
    None
}

async fn transfers(ledger: Rc<RefCell<Ledger>>, n: u64) {
    let sock = os::bind_ephemeral();
    for _ in 0..n {
        let from = sdk::random_choice(4);
        let to = (from + 1 + sdk::random_choice(3)) % 4;
        let amt = sdk::random_range(1, 20);
        let id = {
            let mut l = ledger.borrow_mut();
            l.next += 1;
            format!("t{}", l.next)
        };
        let msg = format!("TX {id} {} {} {amt}", ACCOUNTS[from], ACCOUNTS[to]);
        match rpc(&sock, &id, &msg, 6).await {
            Some(r) if r.starts_with("OK") => {
                ledger.borrow_mut().acked.insert(id.clone(), msg);
            }
            _ => {
                sdk::assert_sometimes(true, S_GAVE_UP, &id);
                // like running ANTITHESIS_STOP_FAULTS: let the system heal a bit
                sdk::stop_faults(20 * MS);
            }
        }
        let acked = ledger.borrow().acked.len();
        sdk::export_state("workload/acked", format!("{acked} acknowledged transfers"));
        os::sleep(sdk::random_range(0, 3 * MS)).await;
    }
}

/// Read the server state and check every acknowledged transfer is present.
async fn check_durable(ledger: &Rc<RefCell<Ledger>>, attempts: u32) -> bool {
    let acked: Vec<String> = ledger.borrow().acked.keys().cloned().collect();
    let sock = os::bind_ephemeral();
    let id = format!("r{}", sdk::get_random() % 1_000_000);
    let Some(state) = rpc(&sock, &id, &format!("READ {id}"), attempts).await else { return false };
    let applied: BTreeSet<&str> =
        state.split(' ').find_map(|f| f.strip_prefix("applied=")).unwrap_or("").split(',').collect();
    let missing: Vec<&String> = acked.iter().filter(|r| !applied.contains(r.as_str())).collect();
    sdk::assert_always(missing.is_empty(), DURABLE, &format!("acknowledged but missing after recovery: {missing:?}"));
    true
}

fn template(ledger: Rc<RefCell<Ledger>>) -> Template {
    let l = ledger.clone();
    let first: MainFn = Rc::new(|| {
        Box::pin(async {
            let sock = os::bind_ephemeral();
            while rpc(&sock, "p0", "PING p0", 1).await.is_none() {}
        })
    });
    let par: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move { transfers(l, sdk::random_range(2, 6)).await })
    });
    let l = ledger.clone();
    let single: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move { transfers(l, 40).await })
    });
    let l = ledger.clone();
    let audit: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move {
            check_durable(&l, 2).await;
        })
    });
    let l = ledger.clone();
    let eventually: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move {
            // restored nodes need time: retry until the server answers
            while !check_durable(&l, 4).await {}
            sdk::assert_reachable(R_EVENTUALLY, "");
        })
    });
    let l = ledger;
    let finally: MainFn = Rc::new(move || {
        let l = l.clone();
        Box::pin(async move {
            while !check_durable(&l, 4).await {}
            sdk::assert_reachable(R_FINALLY, "");
        })
    });
    Template {
        commands: vec![
            cmd("first_wait_for_bank", first),
            cmd("parallel_driver_transfers", par),
            cmd("singleton_driver_long_transfer_run", single),
            cmd("anytime_audit_durability", audit),
            cmd("eventually_recovered_durability", eventually),
            cmd("finally_durability", finally),
        ],
        driver_time: 400 * MS,
        max_parallel: 4,
    }
}

/// Build the machine for one timeline: a faultable `bank` node and a
/// `workload` node that hosts the Test Composer and the test commands.
pub fn machine(bugs: &Bugs, faults: FaultConfig, swarm: bool, buggify: bool) -> MachineSpec {
    let bugs = Rc::new(bugs.clone());
    let main: MainFn = Rc::new(move || Box::pin(server(bugs.clone())));
    let tmpl = Rc::new(template(Rc::new(RefCell::new(Ledger::default()))));
    let composer: MainFn = Rc::new(move || Box::pin(compose(tmpl.clone())));
    MachineSpec {
        nodes: vec![("bank".into(), true, Some(main)), ("workload".into(), false, None)],
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

pub const MAX_TIME: Time = 3_000 * MS;
