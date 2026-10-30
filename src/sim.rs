//! The deterministic machine: virtual clock, single-core scheduler, simulated
//! network and disks, node lifecycle, and the fault injector.
//!
//! A real deterministic hypervisor (Antithesis' Determinator, bedrock, dhyve)
//! gets these properties from VT-x: it traps RDTSC/RDRAND/CPUID, runs the guest
//! on one core, counts retired instructions to define time, and injects
//! interrupts at exact instruction counts. This orb has no KVM, so here the
//! "guest" is async Rust code that may only talk to the outside world through
//! this module's API (the equivalent of hypercalls / trapped instructions).
//! Every await is a point where the "CPU" may switch to another guest thread.

use crate::faults::{FaultConfig, FaultKind, LinkMode};
use crate::sdk::{Kind, PropRun};
use crate::tape::Tape;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

pub type Time = u64; // virtual nanoseconds
pub const US: Time = 1_000;
pub const MS: Time = 1_000_000;
pub type NodeId = usize;
pub type TaskId = u64;
pub type LocalFut = Pin<Box<dyn Future<Output = ()>>>;
pub type MainFn = Rc<dyn Fn() -> LocalFut>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Addr {
    pub node: NodeId,
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub from: Addr,
    pub to: Addr,
    pub body: String,
}

/// Per-run settings that are not part of the experiment's identity.
#[derive(Clone)]
pub struct RunConfig {
    /// Hard stop for the timeline (virtual time).
    pub max_time: Time,
    /// Stop early at this virtual time (used by `inspect` and branch horizons).
    pub stop_at: Option<Time>,
    /// Keep the full human-readable trace (replay); otherwise only its hash.
    pub trace: bool,
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub property: String,
    pub time: Time,
    pub detail: String,
}

pub struct RunResult {
    pub tape: Vec<u64>,
    pub failure: Option<Failure>,
    pub props: BTreeMap<&'static str, PropRun>,
    pub end_time: Time,
    #[allow(dead_code)]
    pub finished: bool,
    pub trace_hash: u64,
    pub trace: Vec<String>,
    /// exact virtual time of each trace line
    pub trace_times: Vec<Time>,
    /// (tape position, time, label) of sometimes/reachable assertions the
    /// first time they fire in this run: candidate branch points.
    pub interesting: Vec<(usize, Time, String)>,
    /// (tape position, time) samples, used to map a moment in time to a tape prefix.
    pub moments: Vec<(usize, Time)>,
    pub fault_counts: BTreeMap<&'static str, u32>,
    pub app_state: BTreeMap<String, String>,
    pub steps: u64,
}

// ---------------------------------------------------------------- disk

/// A per-node disk of byte files with a volatile page cache. Writes land in
/// the cache; only fsync makes them durable. On a crash the "OS" has flushed a
/// tape-chosen prefix of each file's unsynced operations, and the first
/// unflushed write may be torn (partially persisted).
#[derive(Default, Clone)]
pub struct Disk {
    files: BTreeMap<String, FileState>,
}

#[derive(Default, Clone)]
struct FileState {
    durable: Vec<u8>,
    pending: Vec<DiskOp>,
}

#[derive(Clone)]
enum DiskOp {
    Write(u64, Vec<u8>),
    Append(Vec<u8>),
    Truncate(u64),
    /// Atomic whole-file replacement (write temp file + rename).
    Replace(Vec<u8>),
}

fn apply_op(v: &mut Vec<u8>, op: &DiskOp) {
    match op {
        DiskOp::Write(off, data) => {
            let end = *off as usize + data.len();
            if v.len() < end {
                v.resize(end, 0);
            }
            v[*off as usize..end].copy_from_slice(data);
        }
        DiskOp::Append(data) => v.extend_from_slice(data),
        DiskOp::Truncate(len) => v.resize(*len as usize, 0),
        DiskOp::Replace(data) => *v = data.clone(),
    }
}

fn apply_ops(base: &[u8], ops: &[DiskOp]) -> Vec<u8> {
    let mut v = base.to_vec();
    for op in ops {
        apply_op(&mut v, op);
    }
    v
}

impl Disk {
    fn content(&self, f: &str) -> Vec<u8> {
        self.files.get(f).map(|s| apply_ops(&s.durable, &s.pending)).unwrap_or_default()
    }
    /// `content(f)[off..off+len]` (clipped at EOF) without materialising the
    /// whole file: only the requested window is replayed through the pending ops.
    #[allow(dead_code)]
    fn read_range(&self, f: &str, off: u64, len: usize) -> Vec<u8> {
        let Some(s) = self.files.get(f) else { return vec![] };
        let (lo, hi) = (off as usize, off as usize + len);
        // invariant: window bytes at positions >= size are zero
        let mut win = vec![0u8; len];
        let mut size = s.durable.len();
        let copy_in = |win: &mut Vec<u8>, at: usize, d: &[u8]| {
            let (a, b) = (at.max(lo), (at + d.len()).min(hi));
            if a < b {
                win[a - lo..b - lo].copy_from_slice(&d[a - at..b - at]);
            }
        };
        copy_in(&mut win, 0, &s.durable);
        for op in &s.pending {
            match op {
                DiskOp::Write(o, d) => {
                    copy_in(&mut win, *o as usize, d);
                    size = size.max(*o as usize + d.len());
                }
                DiskOp::Append(d) => {
                    copy_in(&mut win, size, d);
                    size += d.len();
                }
                DiskOp::Truncate(l) => {
                    let l = *l as usize;
                    if l < size && l < hi {
                        win[l.max(lo) - lo..].fill(0);
                    }
                    size = l;
                }
                DiskOp::Replace(d) => {
                    win.fill(0);
                    copy_in(&mut win, 0, d);
                    size = d.len();
                }
            }
        }
        win.truncate(size.saturating_sub(lo).min(len));
        win
    }
    #[allow(dead_code)]
    fn len(&self, f: &str) -> u64 {
        let Some(s) = self.files.get(f) else { return 0 };
        let mut size = s.durable.len();
        for op in &s.pending {
            size = match op {
                DiskOp::Write(o, d) => size.max(*o as usize + d.len()),
                DiskOp::Append(d) => size + d.len(),
                DiskOp::Truncate(l) => *l as usize,
                DiskOp::Replace(d) => d.len(),
            };
        }
        size as u64
    }
    fn durable_view(&self) -> BTreeMap<String, (usize, usize)> {
        self.files.iter().map(|(k, s)| (k.clone(), (s.durable.len(), s.pending.len()))).collect()
    }
}

// ---------------------------------------------------------------- world

struct Node {
    name: String,
    faultable: bool,
    up: bool,
    paused_until: Time,
    incarnation: u32,
    disk: Disk,
    sockets: BTreeMap<u16, Mailbox>,
    next_port: u16,
    main: Option<MainFn>,
}

#[derive(Default)]
struct Mailbox {
    queue: VecDeque<Packet>,
    waiter: Option<TaskId>,
}

struct Task {
    node: NodeId,
    name: String,
    fut: Option<LocalFut>,
    system: bool,
    paused_until: Time,
    join_waiters: Vec<TaskId>,
}

enum Event {
    Wake(TaskId),
    Deliver(Packet),
    FaultTick,
    Restart(NodeId),
    Heal,
}

struct Partition {
    /// directed links (from, to) that are disrupted
    cut: BTreeSet<(NodeId, NodeId)>,
    mode: LinkMode,
    until: Time,
}

pub struct FaultState {
    pub cfg: FaultConfig,
    /// True between setup_complete and quiescence.
    pub active: bool,
    pub enabled: BTreeSet<FaultKind>,
    pub paused_until: Time,
    partition: Option<Partition>,
    clogs: BTreeMap<NodeId, (LinkMode, Time)>,
    pub clock_offset: i64,
    pub counts: BTreeMap<&'static str, u32>,
}

pub struct World {
    pub now: Time,
    pub tape: Tape,
    events: BTreeMap<(Time, u64), Event>,
    seq: u64,
    nodes: Vec<Node>,
    tasks: BTreeMap<TaskId, Task>,
    task_results: BTreeMap<TaskId, bool>,
    runnable: BTreeSet<TaskId>,
    next_task: TaskId,
    current: Option<TaskId>,
    graveyard: Vec<LocalFut>,
    to_spawn: Vec<(NodeId, String, MainFn)>,
    pending_cpu: Time,
    cfg: RunConfig,
    pub faults: FaultState,
    pub catalog: BTreeMap<&'static str, Kind>,
    pub props: BTreeMap<&'static str, PropRun>,
    pub failure: Option<Failure>,
    finished: bool,
    setup_done: bool,
    trace: Vec<String>,
    trace_times: Vec<Time>,
    trace_hash: u64,
    pub interesting: Vec<(usize, Time, String)>,
    moments: Vec<(usize, Time)>,
    pub app_state: BTreeMap<String, String>,
    pub buggify_enabled: bool,
    pub buggify_sites: BTreeMap<String, bool>,
    steps: u64,
}

thread_local! {
    static WORLD: RefCell<Option<World>> = const { RefCell::new(None) };
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Access the machine from guest code or from the hypervisor loop.
pub fn with<R>(f: impl FnOnce(&mut World) -> R) -> R {
    WORLD.with(|w| f(w.borrow_mut().as_mut().expect("no simulation running")))
}

impl World {
    pub fn log(&mut self, node: Option<NodeId>, msg: impl AsRef<str>) {
        let who = node.map(|n| self.nodes[n].name.as_str()).unwrap_or("hypervisor");
        let line = format!("[{:>9.3}ms] {:<10} | {}", self.now as f64 / MS as f64, who, msg.as_ref());
        for b in line.bytes() {
            self.trace_hash = (self.trace_hash ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        if self.cfg.trace {
            self.trace.push(line);
            self.trace_times.push(self.now);
        }
    }

    fn schedule(&mut self, at: Time, ev: Event) {
        self.seq += 1;
        self.events.insert((at, self.seq), ev);
    }

    pub fn current_task(&self) -> TaskId {
        self.current.expect("guest API called outside a guest task")
    }
    pub fn current_node(&self) -> NodeId {
        self.tasks[&self.current_task()].node
    }

    fn make_runnable(&mut self, t: TaskId) {
        if self.tasks.contains_key(&t) {
            self.runnable.insert(t);
        }
    }

    pub fn node_name(&self, n: NodeId) -> String {
        self.nodes[n].name.clone()
    }

    pub fn node_id(&self, name: &str) -> NodeId {
        self.nodes.iter().position(|n| n.name == name).unwrap_or_else(|| panic!("no node {name}"))
    }

    fn spawn_task(&mut self, node: NodeId, name: String, fut: LocalFut, system: bool) -> TaskId {
        self.next_task += 1;
        let id = self.next_task;
        self.tasks.insert(id, Task { node, name, fut: Some(fut), system, paused_until: 0, join_waiters: vec![] });
        self.runnable.insert(id);
        id
    }

    fn end_task(&mut self, t: TaskId, ok: bool) {
        if let Some(task) = self.tasks.remove(&t) {
            if let Some(f) = task.fut {
                self.graveyard.push(f);
            }
            for w in task.join_waiters {
                self.make_runnable(w);
            }
            self.task_results.insert(t, ok);
            self.runnable.remove(&t);
        }
    }

    pub fn kill_task(&mut self, t: TaskId) {
        if let Some(task) = self.tasks.get(&t) {
            let msg = format!("killed task '{}'", task.name);
            let node = task.node;
            self.log(Some(node), msg);
        }
        self.end_task(t, false);
    }

    // ---- network

    fn link(&self, a: NodeId, b: NodeId) -> LinkMode {
        let mut mode = LinkMode::Normal;
        if let Some(p) = &self.faults.partition {
            if p.cut.contains(&(a, b)) {
                mode = mode.max(p.mode);
            }
        }
        for n in [a, b] {
            if let Some((m, _)) = self.faults.clogs.get(&n) {
                mode = mode.max(*m);
            }
        }
        mode
    }

    pub fn send(&mut self, p: Packet) {
        let (a, b) = (p.from.node, p.to.node);
        let mode = self.link(a, b);
        let desc = format!("send -> {}:{} \"{}\"", self.nodes[b].name, p.to.port, p.body);
        if mode == LinkMode::Stopped {
            self.log(Some(a), format!("{desc}  [DROPPED: partition]"));
            return;
        }
        let net_faults = self.faults.active && self.faults.enabled.contains(&FaultKind::Latency);
        let fc = self.faults.cfg.clone();
        if net_faults && self.tape.chance(fc.drop_rate) {
            self.log(Some(a), format!("{desc}  [DROPPED: packet loss]"));
            return;
        }
        let mut lat = if net_faults { self.tape.range(fc.latency_min, fc.latency_max) } else { fc.latency_min };
        if mode == LinkMode::Slowed {
            lat += self.tape.range(10 * MS, 50 * MS);
        }
        self.log(Some(a), format!("{desc}  (+{:.2}ms)", lat as f64 / MS as f64));
        if net_faults && self.tape.chance(fc.dup_rate) {
            let extra = self.tape.range(fc.latency_min, fc.latency_max * 4);
            self.log(Some(a), "  [DUPLICATED by network]");
            self.schedule(self.now + extra, Event::Deliver(p.clone()));
        }
        self.schedule(self.now + lat, Event::Deliver(p));
    }

    fn deliver(&mut self, p: Packet) {
        let n = p.to.node;
        if !self.nodes[n].up || self.link(p.from.node, n) == LinkMode::Stopped {
            return;
        }
        if let Some(mb) = self.nodes[n].sockets.get_mut(&p.to.port) {
            let waiter = mb.waiter.take();
            mb.queue.push_back(p);
            if let Some(w) = waiter {
                self.make_runnable(w);
            }
        }
    }

    // ---- node lifecycle

    fn crash_node(&mut self, n: NodeId, graceful: bool) {
        let kind = if graceful { "STOP (graceful)" } else { "KILL (crash)" };
        self.log(None, format!("FAULT node {} {}", kind, self.nodes[n].name));
        let tasks: Vec<TaskId> = self.tasks.iter().filter(|(_, t)| t.node == n).map(|(id, _)| *id).collect();
        for t in tasks {
            self.end_task(t, false);
        }
        let names: Vec<String> = self.nodes[n].disk.files.keys().cloned().collect();
        let torn_on = self.faults.enabled.contains(&FaultKind::TornWrite);
        let torn_rate = self.faults.cfg.torn_write;
        for f in names {
            let st = self.nodes[n].disk.files.get(&f).unwrap().clone();
            let keep = if graceful { st.pending.len() } else { self.tape.range(0, st.pending.len() as u64) as usize };
            let mut durable = apply_ops(&st.durable, &st.pending[..keep]);
            let mut torn = String::new();
            if keep < st.pending.len() && torn_on && self.tape.chance(torn_rate) {
                // the next write was in flight: persist only part of it
                let part = match &st.pending[keep] {
                    DiskOp::Write(off, d) if d.len() > 1 => Some(DiskOp::Write(*off, d[..self.tape.range(1, d.len() as u64 - 1) as usize].to_vec())),
                    DiskOp::Append(d) if d.len() > 1 => Some(DiskOp::Append(d[..self.tape.range(1, d.len() as u64 - 1) as usize].to_vec())),
                    _ => None,
                };
                if let Some(op) = part {
                    apply_op(&mut durable, &op);
                    torn = " (+1 TORN write)".into();
                    *self.faults.counts.entry(FaultKind::TornWrite.name()).or_default() += 1;
                }
            }
            if keep < st.pending.len() {
                self.log(None, format!("  disk {}: lost {} unsynced op(s) of '{}'{}", self.nodes[n].name, st.pending.len() - keep, f, torn));
            }
            let fs = self.nodes[n].disk.files.get_mut(&f).unwrap();
            fs.durable = durable;
            fs.pending.clear();
        }
        let node = &mut self.nodes[n];
        node.up = false;
        node.sockets.clear();
        let fc = self.faults.cfg.clone();
        let delay = self.tape.range(fc.restart_min, fc.restart_max);
        self.schedule(self.now + delay, Event::Restart(n));
    }

    fn restart_node(&mut self, n: NodeId) {
        if self.nodes[n].up {
            return;
        }
        let node = &mut self.nodes[n];
        node.up = true;
        node.incarnation += 1;
        node.paused_until = 0;
        let main = node.main.clone();
        let name = format!("{}#{}", node.name, node.incarnation);
        self.log(Some(n), format!("node (re)started, incarnation {}", self.nodes[n].incarnation));
        if let Some(m) = main {
            self.to_spawn.push((n, name, m));
        }
    }

    // ---- fault injector (consulted every `tick`, so faults are decided "late"
    //      and a branch from any moment can choose different faults)

    fn fault_tick(&mut self) {
        if !self.faults.active {
            return;
        }
        let fc = self.faults.cfg.clone();
        self.schedule(self.now + fc.tick, Event::FaultTick);
        if self.now < self.faults.paused_until {
            return;
        }
        let enabled: Vec<FaultKind> = self.faults.enabled.iter().copied().collect();
        for k in enabled {
            if !self.tape.chance(fc.rate(k)) {
                continue;
            }
            let applied = self.apply_fault(k, &fc);
            if applied {
                *self.faults.counts.entry(k.name()).or_default() += 1;
            }
        }
        // expire clogs / partitions
        let now = self.now;
        let expired: Vec<NodeId> = self.faults.clogs.iter().filter(|(_, (_, u))| *u <= now).map(|(n, _)| *n).collect();
        for n in expired {
            self.faults.clogs.remove(&n);
            self.log(None, format!("network clog on {} ended", self.nodes[n].name));
        }
    }

    fn faultable_up(&self) -> Vec<NodeId> {
        (0..self.nodes.len()).filter(|&n| self.nodes[n].faultable && self.nodes[n].up).collect()
    }

    fn apply_fault(&mut self, k: FaultKind, fc: &FaultConfig) -> bool {
        let n_nodes = self.nodes.len();
        match k {
            // per-packet / per-crash / per-fsync: handled where they happen
            FaultKind::Latency | FaultKind::TornWrite | FaultKind::LyingFsync => false,
            FaultKind::Corrupt => {
                let c = self.faultable_up();
                if c.is_empty() {
                    return false;
                }
                let n = c[self.tape.choice(c.len())];
                let files: Vec<String> =
                    self.nodes[n].disk.files.iter().filter(|(_, f)| !f.durable.is_empty()).map(|(k, _)| k.clone()).collect();
                if files.is_empty() {
                    return false;
                }
                let f = files[self.tape.choice(files.len())].clone();
                let fs = self.nodes[n].disk.files.get_mut(&f).unwrap();
                let i = self.tape.choice(fs.durable.len());
                fs.durable[i] ^= 1 << self.tape.choice(7);
                let msg = format!("FAULT bit rot: flipped a bit at byte {i} of '{f}' on {}", self.nodes[n].name);
                self.log(None, msg);
                true
            }
            FaultKind::Partition => {
                if self.faults.partition.is_some() {
                    return false;
                }
                let mut cut = BTreeSet::new();
                let shape;
                if self.tape.chance(0.5) {
                    // full partition: two groups that cannot talk to each other
                    let group: Vec<bool> = (0..n_nodes).map(|_| self.tape.chance(0.5)).collect();
                    for a in 0..n_nodes {
                        for b in 0..n_nodes {
                            if group[a] != group[b] {
                                cut.insert((a, b));
                            }
                        }
                    }
                    let names: Vec<&str> = (0..n_nodes).filter(|&i| group[i]).map(|i| self.nodes[i].name.as_str()).collect();
                    shape = format!("{{{}}} | rest", names.join(","));
                } else {
                    // partial (and maybe one-way) partition: one node loses some of its links
                    let x = self.tape.choice(n_nodes);
                    let dir = self.tape.choice(3); // 0 both ways, 1 outbound only, 2 inbound only
                    let mut peers = vec![];
                    for y in (0..n_nodes).filter(|&y| y != x) {
                        if self.tape.chance(0.5) {
                            if dir != 2 {
                                cut.insert((x, y));
                            }
                            if dir != 1 {
                                cut.insert((y, x));
                            }
                            peers.push(self.nodes[y].name.as_str());
                        }
                    }
                    let arrow = ["<->", "->", "<-"][dir];
                    shape = format!("partial: {} {arrow} {{{}}}", self.nodes[x].name, peers.join(","));
                }
                if cut.is_empty() {
                    return false;
                }
                let mode = if self.tape.chance(0.5) { LinkMode::Slowed } else { LinkMode::Stopped };
                let until = self.now + self.tape.range(fc.partition_min, fc.partition_max);
                let msg = format!("FAULT partition {:?} {shape}, for {:.1}ms", mode, (until - self.now) as f64 / MS as f64);
                self.faults.partition = Some(Partition { cut, mode, until });
                self.schedule(until, Event::Heal);
                self.log(None, msg);
                true
            }
            FaultKind::Clog => {
                let n = self.tape.choice(n_nodes);
                let mode = if self.tape.chance(0.5) { LinkMode::Slowed } else { LinkMode::Stopped };
                let until = self.now + self.tape.range(fc.partition_min, fc.partition_max);
                self.faults.clogs.insert(n, (mode, until));
                let msg = format!("FAULT clog {:?} on {} for {:.1}ms", mode, self.nodes[n].name, (until - self.now) as f64 / MS as f64);
                self.log(None, msg);
                true
            }
            FaultKind::Kill | FaultKind::Stop => {
                let c = self.faultable_up();
                if c.is_empty() {
                    return false;
                }
                let n = c[self.tape.choice(c.len())];
                self.crash_node(n, k == FaultKind::Stop);
                true
            }
            FaultKind::Pause => {
                let c = self.faultable_up();
                if c.is_empty() {
                    return false;
                }
                let n = c[self.tape.choice(c.len())];
                let until = self.now + self.tape.range(fc.pause_min, fc.pause_max);
                self.nodes[n].paused_until = until;
                self.schedule(until, Event::Heal);
                let msg = format!("FAULT pause node {} for {:.1}ms", self.nodes[n].name, (until - self.now) as f64 / MS as f64);
                self.log(None, msg);
                true
            }
            FaultKind::ThreadPause => {
                let c: Vec<TaskId> = self.tasks.iter().filter(|(_, t)| !t.system).map(|(id, _)| *id).collect();
                if c.is_empty() {
                    return false;
                }
                let t = c[self.tape.choice(c.len())];
                let until = self.now + self.tape.range(fc.pause_min / 4, fc.pause_max / 4);
                let task = self.tasks.get_mut(&t).unwrap();
                task.paused_until = until;
                let (node, name) = (task.node, task.name.clone());
                self.schedule(until, Event::Wake(t));
                self.log(Some(node), format!("FAULT thread pause '{}' for {:.2}ms", name, (until - self.now) as f64 / MS as f64));
                true
            }
            FaultKind::ClockSkip => {
                let max = fc.clock_skip_max as i64;
                let delta = self.tape.range(0, 2 * max as u64) as i64 - max;
                self.faults.clock_offset += delta;
                self.log(None, format!("FAULT wall-clock skip {:+.1}ms", delta as f64 / MS as f64));
                true
            }
        }
    }

    fn heal(&mut self) {
        let now = self.now;
        if self.faults.partition.as_ref().is_some_and(|p| p.until <= now) {
            self.faults.partition = None;
            self.log(None, "partition healed");
        }
    }

    /// Stop faults (permanently or until `until`), heal the network and
    /// restore crashed nodes. Used by quiescent commands and stop_faults().
    pub fn calm(&mut self, until: Option<Time>) {
        match until {
            None => self.faults.active = false,
            Some(t) => self.faults.paused_until = self.faults.paused_until.max(t),
        }
        self.faults.partition = None;
        self.faults.clogs.clear();
        for n in 0..self.nodes.len() {
            self.nodes[n].paused_until = 0;
            if !self.nodes[n].up {
                self.restart_node(n);
            }
        }
        for t in self.tasks.values_mut() {
            t.paused_until = 0;
        }
    }

    /// The SDK's setup_complete(): the system is ready for the test.
    pub fn setup_complete(&mut self) {
        if !self.setup_done {
            self.setup_done = true;
            self.log(None, "setup_complete received");
        }
    }
    pub fn setup_done(&self) -> bool {
        self.setup_done
    }

    /// Called by the Test Composer once the first_ command is done.
    pub fn start_faults(&mut self) {
        if self.faults.active {
            return;
        }
        self.faults.active = true;
        self.log(None, format!("fault injection ON: {:?}", self.faults.enabled));
        let tick = self.faults.cfg.tick;
        self.schedule(self.now + tick, Event::FaultTick);
    }

    pub fn finish(&mut self) {
        self.finished = true;
    }

    #[allow(dead_code)]
    pub fn wall_clock(&self) -> i64 {
        self.now as i64 + self.faults.clock_offset
    }

    #[allow(dead_code)]
    pub fn charge_cpu(&mut self, ns: Time) {
        self.pending_cpu += ns;
    }

    pub fn fail(&mut self, property: &str, detail: String) {
        if self.failure.is_none() {
            self.log(None, format!("PROPERTY VIOLATED: \"{property}\" {detail}"));
            self.failure = Some(Failure { property: property.to_string(), time: self.now, detail });
        }
    }

    pub fn mark_interesting(&mut self, label: &str) {
        let pos = self.tape.pos();
        self.interesting.push((pos, self.now, label.to_string()));
    }

    fn snapshot_state(&self) -> BTreeMap<String, String> {
        let mut m = self.app_state.clone();
        for n in &self.nodes {
            let disk: Vec<String> = n
                .disk
                .durable_view()
                .iter()
                .map(|(f, (d, p))| format!("{f}: {d} durable bytes, {p} unsynced op(s)"))
                .collect();
            m.insert(
                format!("node/{}", n.name),
                format!(
                    "{} incarnation={} open sockets={} disk=[{}]",
                    if n.up { "UP" } else { "DOWN (its exported state above is from before the crash)" },
                    n.incarnation,
                    n.sockets.len(),
                    disk.join("; ")
                ),
            );
        }
        m
    }
}

// ---------------------------------------------------------------- the run loop

fn noop_waker() -> Waker {
    use std::task::{RawWaker, RawWakerVTable};
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// Builder used by an app to describe the machine before it boots.
pub struct MachineSpec {
    pub nodes: Vec<(String, bool, Option<MainFn>)>,
    pub catalog: Vec<(&'static str, Kind)>,
    /// Hypervisor-level task (the Test Composer) and the node it runs on.
    pub composer: (String, MainFn),
    pub faults: FaultConfig,
    pub swarm: bool,
    pub buggify: bool,
}

pub const NO_PANIC: &str = "No guest process panicked";

pub fn run(spec: &MachineSpec, tape: Tape, cfg: RunConfig) -> RunResult {
    install_panic_hook();
    let mut catalog: BTreeMap<&'static str, Kind> = spec.catalog.iter().cloned().collect();
    catalog.insert(NO_PANIC, Kind::Always);
    let world = World {
        now: 0,
        tape,
        events: BTreeMap::new(),
        seq: 0,
        nodes: spec
            .nodes
            .iter()
            .map(|(name, faultable, main)| Node {
                name: name.clone(),
                faultable: *faultable,
                up: false,
                paused_until: 0,
                incarnation: 0,
                disk: Disk::default(),
                sockets: BTreeMap::new(),
                next_port: 40000,
                main: main.clone(),
            })
            .collect(),
        tasks: BTreeMap::new(),
        task_results: BTreeMap::new(),
        runnable: BTreeSet::new(),
        next_task: 0,
        current: None,
        graveyard: vec![],
        to_spawn: vec![],
        pending_cpu: 0,
        cfg: cfg.clone(),
        faults: FaultState {
            cfg: spec.faults.clone(),
            active: false,
            enabled: BTreeSet::new(),
            paused_until: 0,
            partition: None,
            clogs: BTreeMap::new(),
            clock_offset: 0,
            counts: BTreeMap::new(),
        },
        catalog,
        props: BTreeMap::new(),
        failure: None,
        finished: false,
        setup_done: false,
        trace: vec![],
        trace_times: vec![],
        trace_hash: 0xcbf2_9ce4_8422_2325,
        interesting: vec![],
        moments: vec![],
        app_state: BTreeMap::new(),
        buggify_enabled: spec.buggify,
        buggify_sites: BTreeMap::new(),
        steps: 0,
    };
    WORLD.with(|w| *w.borrow_mut() = Some(world));

    // Boot: choose the swarm configuration, start every node, start the composer.
    with(|w| {
        let all = spec.faults.kinds();
        for k in all {
            let on = !spec.swarm || w.tape.chance(0.6);
            if on {
                w.faults.enabled.insert(k);
            }
        }
        if spec.swarm {
            w.log(None, format!("swarm testing: this timeline enables {:?}", w.faults.enabled));
        }
        for n in 0..w.nodes.len() {
            w.restart_node(n);
        }
        let comp_node = w.node_id(&spec.composer.0);
        w.to_spawn.push((comp_node, "test-composer".into(), spec.composer.1.clone()));
    });

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        // spawn pending main()s outside of the world borrow
        let spawns = with(|w| std::mem::take(&mut w.to_spawn));
        for (node, name, main) in spawns {
            let fut = main();
            with(|w| {
                let system = name == "test-composer";
                w.spawn_task(node, name, fut, system);
            });
        }
        let dead = with(|w| std::mem::take(&mut w.graveyard));
        drop(dead);

        let next = with(|w| -> Option<Option<TaskId>> {
            if w.failure.is_some() || w.finished || w.now >= w.cfg.max_time {
                return None;
            }
            if w.cfg.stop_at.is_some_and(|t| w.now > t) {
                return None;
            }
            // fire every event that is due
            while let Some((&key, _)) = w.events.iter().next() {
                if key.0 > w.now {
                    break;
                }
                let ev = w.events.remove(&key).unwrap();
                match ev {
                    Event::Wake(t) => w.make_runnable(t),
                    Event::Deliver(p) => w.deliver(p),
                    Event::FaultTick => w.fault_tick(),
                    Event::Restart(n) => w.restart_node(n),
                    Event::Heal => w.heal(),
                }
                // a moment = the machine after everything due at this time happened
                let pos = w.tape.pos();
                w.moments.push((pos, w.now));
                if !w.to_spawn.is_empty() {
                    return Some(None); // go spawn, then come back
                }
            }
            let now = w.now;
            let candidates: Vec<TaskId> = w
                .runnable
                .iter()
                .copied()
                .filter(|t| {
                    let task = &w.tasks[t];
                    let node = &w.nodes[task.node];
                    node.up && node.paused_until <= now && task.paused_until <= now
                })
                .collect();
            if candidates.is_empty() {
                // the "HLT" case: jump virtual time to the next event
                match w.events.keys().next() {
                    Some(&(t, _)) => {
                        w.now = w.now.max(t);
                        return Some(None);
                    }
                    None => {
                        w.log(None, "machine idle with nothing scheduled: halting");
                        return None;
                    }
                }
            }
            let pos = w.tape.pos();
            w.moments.push((pos, now));
            let pick = candidates[w.tape.choice(candidates.len())];
            Some(Some(pick))
        });
        let tid = match next {
            None => break,
            Some(None) => continue,
            Some(Some(t)) => t,
        };

        let fut = with(|w| {
            w.steps += 1;
            w.current = Some(tid);
            w.runnable.remove(&tid);
            w.tasks.get_mut(&tid).and_then(|t| t.fut.take())
        });
        let Some(mut fut) = fut else { continue };
        IN_GUEST.with(|g| *g.borrow_mut() = true);
        let res = catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(&mut cx)));
        IN_GUEST.with(|g| *g.borrow_mut() = false);
        let leftover = with(|w| {
            w.current = None;
            // every scheduling quantum costs virtual time, like instructions retired
            w.now += 2 * US + std::mem::take(&mut w.pending_cpu);
            match res {
                Ok(Poll::Pending) => match w.tasks.get_mut(&tid) {
                    Some(t) => {
                        t.fut = Some(fut);
                        None
                    }
                    None => Some(fut),
                },
                Ok(Poll::Ready(())) => {
                    w.end_task(tid, true);
                    Some(fut)
                }
                Err(_) => {
                    let msg = LAST_PANIC.with(|p| p.borrow_mut().take()).unwrap_or_default();
                    let name = w.tasks.get(&tid).map(|t| t.name.clone()).unwrap_or_default();
                    w.fail(NO_PANIC, format!("task '{name}' panicked: {msg}"));
                    w.end_task(tid, false);
                    Some(fut)
                }
            }
        });
        drop(leftover);
    }

    let mut w = WORLD.with(|w| w.borrow_mut().take()).unwrap();
    let dead = std::mem::take(&mut w.graveyard);
    let tasks = std::mem::take(&mut w.tasks);
    drop(dead);
    drop(tasks);
    if w.failure.is_none() {
        let e = w.props.entry(NO_PANIC).or_default();
        e.hits += 1;
        e.trues += 1;
    }
    let app_state = w.snapshot_state();
    RunResult {
        tape: std::mem::take(&mut w.tape.recorded),
        failure: w.failure.take(),
        props: std::mem::take(&mut w.props),
        end_time: w.now,
        finished: w.finished,
        trace_hash: w.trace_hash,
        trace: std::mem::take(&mut w.trace),
        trace_times: std::mem::take(&mut w.trace_times),
        interesting: std::mem::take(&mut w.interesting),
        moments: std::mem::take(&mut w.moments),
        fault_counts: w.faults.counts.iter().filter(|(k, _)| !k.starts_with("__")).map(|(k, v)| (*k, *v)).collect(),
        app_state,
        steps: w.steps,
    }
}

thread_local! { static IN_GUEST: RefCell<bool> = const { RefCell::new(false) }; }

fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if IN_GUEST.with(|g| *g.borrow()) {
                let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = info.payload().downcast_ref::<String>() {
                    s.clone()
                } else {
                    "panic".into()
                };
                LAST_PANIC.with(|p| *p.borrow_mut() = Some(msg));
            } else {
                default(info);
            }
        }));
    });
}

// ================================================================ guest API
// Everything below is what guest code is allowed to use. In a real
// deterministic hypervisor these would be trapped instructions, virtual
// devices, and VMCALL hypercalls.

pub mod os {
    use super::*;

    /// Monotonic virtual time (what a trapped RDTSC returns).
    pub fn now() -> Time {
        with(|w| w.now)
    }
    /// Wall-clock time: subject to clock-skip faults.
    #[allow(dead_code)]
    pub fn wall_clock() -> i64 {
        with(|w| w.wall_clock())
    }
    /// Burn virtual CPU time.
    #[allow(dead_code)]
    pub fn cpu(ns: Time) {
        with(|w| w.charge_cpu(ns))
    }
    pub fn log(msg: impl AsRef<str>) {
        with(|w| {
            let n = w.current_node();
            w.log(Some(n), msg)
        })
    }
    pub fn node_id(name: &str) -> NodeId {
        with(|w| w.node_id(name))
    }
    /// (node, incarnation) of the running guest task, or None outside guest
    /// code (e.g. destructors of a crashed node's state). Lets a storage
    /// engine's IO layer refuse calls from a dead incarnation.
    #[allow(dead_code)]
    pub fn context() -> Option<(NodeId, u32)> {
        WORLD.with(|w| {
            let b = w.try_borrow().ok()?;
            let w = b.as_ref()?;
            let n = w.tasks.get(&w.current?)?.node;
            Some((n, w.nodes[n].incarnation))
        })
    }

    pub struct JoinHandle(pub TaskId);

    /// Start a new guest thread on the current node.
    pub fn spawn(name: impl Into<String>, fut: impl Future<Output = ()> + 'static) -> JoinHandle {
        let name = name.into();
        JoinHandle(with(|w| {
            let n = w.current_node();
            w.spawn_task(n, name, Box::pin(fut), false)
        }))
    }

    pub fn kill(h: &JoinHandle) {
        with(|w| w.kill_task(h.0))
    }

    impl JoinHandle {
        pub fn is_done(&self) -> bool {
            with(|w| !w.tasks.contains_key(&self.0))
        }
        /// Resolves to true if the task completed, false if it was killed.
        pub fn join(&self) -> impl Future<Output = bool> {
            let id = self.0;
            std::future::poll_fn(move |_| {
                with(|w| {
                    if w.tasks.contains_key(&id) {
                        let me = w.current_task();
                        w.tasks.get_mut(&id).unwrap().join_waiters.push(me);
                        Poll::Pending
                    } else {
                        Poll::Ready(*w.task_results.get(&id).unwrap_or(&false))
                    }
                })
            })
        }
    }

    /// Let the scheduler run something else (a preemption point).
    #[allow(dead_code)]
    pub fn yield_now() -> impl Future<Output = ()> {
        let mut yielded = false;
        std::future::poll_fn(move |_| {
            if yielded {
                return Poll::Ready(());
            }
            yielded = true;
            with(|w| {
                let me = w.current_task();
                w.make_runnable(me);
            });
            Poll::Pending
        })
    }

    pub fn sleep(d: Time) -> impl Future<Output = ()> {
        let until = now() + d;
        let mut registered = false;
        std::future::poll_fn(move |_| {
            with(|w| {
                if w.now >= until {
                    return Poll::Ready(());
                }
                if !registered {
                    registered = true;
                    let me = w.current_task();
                    w.schedule(until, Event::Wake(me));
                }
                Poll::Pending
            })
        })
    }

    // ---- network

    #[derive(Clone, Copy)]
    pub struct Socket {
        pub addr: Addr,
    }

    pub fn bind(port: u16) -> Socket {
        with(|w| {
            let n = w.current_node();
            w.nodes[n].sockets.entry(port).or_default();
            Socket { addr: Addr { node: n, port } }
        })
    }
    pub fn bind_ephemeral() -> Socket {
        let port = with(|w| {
            let n = w.current_node();
            w.nodes[n].next_port += 1;
            w.nodes[n].next_port
        });
        bind(port)
    }

    impl Socket {
        pub fn send(&self, to: Addr, body: impl Into<String>) {
            let p = Packet { from: self.addr, to, body: body.into() };
            with(|w| w.send(p));
        }
        pub async fn recv(&self) -> Packet {
            loop {
                if let Some(p) = self.recv_timeout(Time::MAX / 4).await {
                    return p;
                }
            }
        }
        /// None on timeout.
        pub fn recv_timeout(&self, d: Time) -> impl Future<Output = Option<Packet>> {
            let addr = self.addr;
            let deadline = now().saturating_add(d);
            let mut timer = false;
            std::future::poll_fn(move |_| {
                with(|w| {
                    let me = w.current_task();
                    let mb = w.nodes[addr.node].sockets.entry(addr.port).or_default();
                    if let Some(p) = mb.queue.pop_front() {
                        return Poll::Ready(Some(p));
                    }
                    mb.waiter = Some(me);
                    if w.now >= deadline {
                        return Poll::Ready(None);
                    }
                    if !timer {
                        timer = true;
                        w.schedule(deadline, Event::Wake(me));
                    }
                    Poll::Pending
                })
            })
        }
    }

    // ---- disk (a virtual block device with a volatile write cache)

    fn disk_op(file: &str, op: DiskOp) {
        with(|w| {
            let n = w.current_node();
            w.nodes[n].disk.files.entry(file.to_string()).or_default().pending.push(op);
        })
    }
    /// Append one line (a '\n' is added) to a line-oriented file.
    pub fn append(file: &str, line: impl Into<String>) {
        let mut b = line.into().into_bytes();
        b.push(b'\n');
        disk_op(file, DiskOp::Append(b))
    }
    /// Atomically replace the whole file with these lines (temp + rename).
    pub fn replace(file: &str, lines: Vec<String>) {
        let mut b = Vec::new();
        for l in lines {
            b.extend_from_slice(l.as_bytes());
            b.push(b'\n');
        }
        disk_op(file, DiskOp::Replace(b))
    }
    /// The file's lines as the guest sees them (page cache included). A torn
    /// or corrupted tail shows up as a malformed last line.
    pub fn read(file: &str) -> Vec<String> {
        let bytes = read_bytes(file);
        let mut v: Vec<String> = String::from_utf8_lossy(&bytes).split('\n').map(String::from).collect();
        if v.last().is_some_and(|l| l.is_empty()) {
            v.pop();
        }
        v
    }

    // byte-level file API (for storage engines such as Turso's pager)
    pub fn read_bytes(file: &str) -> Vec<u8> {
        with(|w| {
            let n = w.current_node();
            w.nodes[n].disk.content(file)
        })
    }
    pub fn pread(file: &str, off: u64, len: usize) -> Vec<u8> {
        with(|w| {
            let n = w.current_node();
            w.nodes[n].disk.read_range(file, off, len)
        })
    }
    pub fn pwrite(file: &str, off: u64, data: &[u8]) {
        disk_op(file, DiskOp::Write(off, data.to_vec()))
    }
    pub fn truncate(file: &str, len: u64) {
        disk_op(file, DiskOp::Truncate(len))
    }
    pub fn file_size(file: &str) -> u64 {
        with(|w| {
            let n = w.current_node();
            w.nodes[n].disk.len(file)
        })
    }
    pub fn exists(file: &str) -> bool {
        with(|w| {
            let n = w.current_node();
            w.nodes[n].disk.files.contains_key(file)
        })
    }

    /// Two-phase fsync for callers that cannot await (e.g. a storage engine's
    /// IO layer): `fsync_begin` snapshots what must become durable,
    /// `fsync_finish` makes it durable, unless the node crashed in between or
    /// a lying-fsync fault fires.
    pub struct SyncToken {
        node: NodeId,
        incarnation: u32,
        file: String,
        count: usize,
    }
    pub fn fsync_begin(file: &str) -> SyncToken {
        with(|w| {
            let n = w.current_node();
            let count = w.nodes[n].disk.files.get(file).map(|f| f.pending.len()).unwrap_or(0);
            SyncToken { node: n, incarnation: w.nodes[n].incarnation, file: file.to_string(), count }
        })
    }
    pub fn fsync_finish(t: SyncToken) {
        with(|w| {
            let n = t.node;
            if w.nodes[n].incarnation != t.incarnation || !w.nodes[n].up {
                return;
            }
            if w.faults.active && w.faults.enabled.contains(&FaultKind::LyingFsync) {
                let p = w.faults.cfg.lying_fsync;
                if w.tape.chance(p) {
                    *w.faults.counts.entry(FaultKind::LyingFsync.name()).or_default() += 1;
                    w.log(Some(n), format!("FAULT lying fsync on '{}' (returned success, persisted nothing)", t.file));
                    return;
                }
            }
            if let Some(fs) = w.nodes[n].disk.files.get_mut(&t.file) {
                let k = t.count.min(fs.pending.len());
                let ops: Vec<DiskOp> = fs.pending.drain(..k).collect();
                fs.durable = apply_ops(&fs.durable, &ops);
            }
        })
    }
    /// Make everything written to `file` so far durable. Takes virtual time;
    /// a crash before it returns keeps only what the OS happened to flush.
    pub async fn fsync(file: &str) {
        let t = fsync_begin(file);
        sleep(200 * US).await;
        fsync_finish(t);
    }
}
