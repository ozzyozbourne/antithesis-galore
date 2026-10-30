//! A replicated system under test: three nodes running a compact Raft
//! (terms, votes, log replication, majority commit, leader leases for reads)
//! with persistent, checksummed state. This is where partitions, clock skips
//! and crashes during elections matter.
//!
//! Switchable bugs, each a classic consensus mistake:
//!
//! * `no_quorum`  - the leader acknowledges a write once it is in its *own* log
//!                  (async replication). A leader crash or partition loses it.
//! * `vote_twice` - a vote is granted without checking whom this node already
//!                  voted for in this term: a split vote can elect two leaders.
//! * `log_check`  - votes are granted without the "candidate's log is at least
//!                  as up to date" check: a stale node becomes leader and
//!                  overwrites committed entries.
//! * `lease_clock`- read leases are measured with the wall clock, so a clock
//!                  skip lets a deposed leader serve stale reads.
//!
//! Global safety properties are checked by a test-only "ghost" oracle that
//! every node reports into (the same trick FoundationDB-style simulations use
//! for whole-system invariants).

use crate::apps::App;
use crate::bank::{seal, unseal};
use crate::composer::{cmd, compose, CmdKind, Template};
use crate::faults::FaultConfig;
use crate::sdk::{self, Kind};
use crate::sim::{os, Addr, MachineSpec, MainFn, NodeId, Time, MS};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

pub static APP: App = App {
    name: "cluster",
    about: "3-node Raft-style replicated log with leader leases",
    bugs: &[
        ("no_quorum", "acknowledge writes before a majority has them"),
        ("vote_twice", "grant a vote without checking votedFor for this term"),
        ("log_check", "grant votes without the log up-to-date check"),
        ("lease_clock", "read leases measured with the (skippable) wall clock"),
    ],
    catalog,
    commands,
    machine,
    max_time: 3_000 * MS,
};

pub const ONE_LEADER: &str = "At most one leader per term";
pub const COMMITTED_STABLE: &str = "Committed entries never change";
pub const ACKED_DURABLE: &str = "Acknowledged writes are never lost";
pub const NO_STALE_READ: &str = "Reads are never stale";
pub const S_ELECTED: &str = "A leader was elected";
pub const S_LEADER_CHANGED: &str = "Leadership moved to another node";
pub const S_REPAIRED: &str = "A follower's conflicting log was repaired";
pub const S_REELECTION: &str = "A candidate's election timed out and it retried";
pub const S_LEASE_REFUSED: &str = "A leader refused a read because its lease expired";
pub const S_REDIRECT: &str = "A client was redirected to the leader";
pub const S_CAUGHT_UP: &str = "A restarted node caught up from the leader";
pub const S_CLOCK_BACK: &str = "A node saw its wall clock go backwards";
pub const S_LATE_LEASE: &str = "A leader served a read late in its lease";
pub const R_EVENTUALLY: &str = "eventually_ check ran";
pub const R_FINALLY: &str = "finally_ check ran";

fn catalog() -> Vec<(&'static str, Kind)> {
    vec![
        (ONE_LEADER, Kind::Always),
        (COMMITTED_STABLE, Kind::Always),
        (ACKED_DURABLE, Kind::Always),
        (NO_STALE_READ, Kind::Always),
        (S_ELECTED, Kind::Sometimes),
        (S_LEADER_CHANGED, Kind::Sometimes),
        (S_REPAIRED, Kind::Sometimes),
        (S_REELECTION, Kind::Sometimes),
        (S_LEASE_REFUSED, Kind::Sometimes),
        (S_REDIRECT, Kind::Sometimes),
        (S_CAUGHT_UP, Kind::Sometimes),
        (S_CLOCK_BACK, Kind::Sometimes),
        (S_LATE_LEASE, Kind::Sometimes),
        (R_EVENTUALLY, Kind::Reachable),
        (R_FINALLY, Kind::Reachable),
    ]
}

const NODES: [&str; 3] = ["n1", "n2", "n3"];
const PORT: u16 = 7000;
const HEARTBEAT: Time = 5 * MS;
const ELECTION_MIN: Time = 40 * MS;
const ELECTION_MAX: Time = 80 * MS;
/// Must be shorter than the minimum election timeout.
const LEASE: Time = 30 * MS;

#[derive(Clone, Default)]
struct Bugs {
    no_quorum: bool,
    vote_twice: bool,
    log_check: bool,
    lease_clock: bool,
}

/// Test-only oracle shared by all nodes (it survives their crashes).
#[derive(Default)]
struct Ghost {
    leaders: BTreeMap<u64, usize>,
    committed: BTreeMap<usize, String>,
    last_leader: Option<usize>,
}

// ------------------------------------------------------------------ node

#[derive(Clone, PartialEq, Debug)]
enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Clone)]
struct Entry {
    term: u64,
    token: String,
}

struct Node {
    me: usize,
    peers: Vec<NodeId>,
    bugs: Bugs,
    ghost: Rc<RefCell<Ghost>>,
    sock: os::Socket,
    // persistent
    term: u64,
    voted_for: Option<usize>,
    log: Vec<Entry>,
    // volatile
    role: Role,
    commit: usize,
    applied: usize,
    tokens: BTreeSet<String>,
    leader_hint: Option<usize>,
    election_deadline: Time,
    votes: BTreeSet<usize>,
    next_hb: Time,
    next_idx: [usize; 3],
    match_len: [usize; 3],
    /// per follower: timestamp of the latest heartbeat it acknowledged
    acked_ts: [i64; 3],
    pending: Vec<(usize, String, Addr)>,
    was_candidate: bool,
    restarted: bool,
    last_wall: i64,
}

fn idx_of(name: &str) -> usize {
    NODES.iter().position(|n| *n == name).unwrap()
}

impl Node {
    fn addr(&self, i: usize) -> Addr {
        Addr { node: self.peers[i], port: PORT }
    }
    fn last(&self) -> (usize, u64) {
        (self.log.len(), self.log.last().map(|e| e.term).unwrap_or(0))
    }
    fn clock(&self) -> i64 {
        if self.bugs.lease_clock {
            os::wall_clock() // BUG(lease_clock)
        } else {
            os::now() as i64
        }
    }
    fn reset_election(&mut self) {
        self.election_deadline = os::now() + sdk::random_range(ELECTION_MIN, ELECTION_MAX);
    }

    async fn persist_meta(&mut self, sync: bool) {
        let v = self.voted_for.map(|v| v.to_string()).unwrap_or("-".into());
        os::replace("meta", vec![seal(format!("term {} vote {v}", self.term))]);
        if sync {
            os::fsync("meta").await;
        }
    }
    async fn persist_log_full(&mut self) {
        os::replace("log", self.log.iter().map(|e| seal(format!("e {} {}", e.term, e.token))).collect());
        os::fsync("log").await;
    }

    async fn step_down(&mut self, term: u64) {
        if term > self.term {
            self.term = term;
            self.voted_for = None;
            self.persist_meta(true).await;
        }
        if self.role != Role::Follower {
            os::log(format!("step down to follower (term {})", self.term));
        }
        self.role = Role::Follower;
        self.pending.clear();
    }

    fn has_lease(&self) -> bool {
        let mut ts: Vec<i64> = (0..3).filter(|&i| i != self.me).map(|i| self.acked_ts[i]).collect();
        ts.sort();
        // with 3 nodes, me + the most recent follower ack form a majority
        let newest = *ts.last().unwrap();
        self.clock() < newest + LEASE as i64
    }
    fn committed_in_term(&self) -> bool {
        self.commit > 0 && self.log[self.commit - 1].term == self.term
    }
    fn value(&self) -> Vec<&String> {
        self.tokens.iter().filter(|t| t.starts_with('w')).collect()
    }

    fn apply_committed(&mut self) {
        while self.applied < self.commit {
            let i = self.applied;
            let tok = self.log[i].token.clone();
            {
                let mut g = self.ghost.borrow_mut();
                match g.committed.get(&(i + 1)) {
                    Some(prev) => sdk::assert_always(
                        *prev == tok,
                        COMMITTED_STABLE,
                        &format!("index {} committed as {prev} earlier, now {tok} on {}", i + 1, NODES[self.me]),
                    ),
                    None => {
                        g.committed.insert(i + 1, tok.clone());
                    }
                }
            }
            self.tokens.insert(tok);
            self.applied += 1;
        }
        // answer clients whose writes are now committed
        let commit = self.commit;
        let mut keep = vec![];
        for (idx, tok, to) in std::mem::take(&mut self.pending) {
            if idx <= commit {
                if self.log[idx - 1].token == tok {
                    self.sock.send(to, format!("OK {tok}"));
                }
            } else {
                keep.push((idx, tok, to));
            }
        }
        self.pending = keep;
        let v = self.value().len();
        sdk::export_state(&format!("{}/state", NODES[self.me]), format!(
            "{:?} term={} log={} commit={} value={v}",
            self.role, self.term, self.log.len(), self.commit
        ));
    }

    fn send_append(&mut self, i: usize) {
        let ni = self.next_idx[i].max(1);
        let prev = ni - 1;
        let prev_term = if prev == 0 { 0 } else { self.log[prev - 1].term };
        let ents: Vec<String> =
            self.log[prev..].iter().take(16).map(|e| format!("{}:{}", e.term, e.token)).collect();
        let ts = self.clock();
        let msg = format!("AE {} {} {prev} {prev_term} {} {ts} {}", self.term, self.me, self.commit, ents.join(","));
        self.sock.send(self.addr(i), msg.trim_end().to_string());
    }

    fn advance_commit(&mut self) {
        let mut m = self.match_len;
        m[self.me] = self.log.len();
        let mut sorted = m.to_vec();
        sorted.sort();
        let majority = sorted[1]; // 2 of 3 have at least this much
        if majority > self.commit && self.log[majority - 1].term == self.term {
            self.commit = majority;
            self.apply_committed();
        }
    }

    async fn become_leader(&mut self) {
        self.role = Role::Leader;
        self.leader_hint = Some(self.me);
        os::log(format!("became LEADER of term {}", self.term));
        sdk::assert_sometimes(true, S_ELECTED, "");
        {
            let mut g = self.ghost.borrow_mut();
            let other = g.leaders.get(&self.term).copied();
            sdk::assert_always(
                other.is_none_or(|o| o == self.me),
                ONE_LEADER,
                &format!("{} and {} both led term {}", NODES[other.unwrap_or(self.me)], NODES[self.me], self.term),
            );
            g.leaders.insert(self.term, self.me);
            if g.last_leader.is_some_and(|l| l != self.me) {
                sdk::assert_sometimes(true, S_LEADER_CHANGED, "");
            }
            g.last_leader = Some(self.me);
        }
        self.next_idx = [self.log.len() + 1; 3];
        self.match_len = [0; 3];
        self.acked_ts = [i64::MIN / 2; 3];
        // a no-op entry lets the new leader commit entries of earlier terms
        self.log.push(Entry { term: self.term, token: format!("noop{}", self.term) });
        os::append("log", seal(format!("e {} noop{}", self.term, self.term)));
        os::fsync("log").await;
        self.next_hb = 0;
    }

    async fn tick(&mut self) {
        let now = os::now();
        match self.role {
            Role::Leader => {
                if now >= self.next_hb {
                    for i in 0..3 {
                        if i != self.me {
                            self.send_append(i);
                        }
                    }
                    self.next_hb = now + HEARTBEAT;
                }
            }
            _ => {
                if self.role == Role::Candidate && now >= self.next_hb && now < self.election_deadline {
                    // RPCs are retried: re-ask peers that have not granted their vote yet
                    let (li, lt) = self.last();
                    for i in 0..3 {
                        if !self.votes.contains(&i) {
                            self.sock.send(self.addr(i), format!("RV {} {} {li} {lt}", self.term, self.me));
                        }
                    }
                    self.next_hb = now + HEARTBEAT;
                }
                if now >= self.election_deadline {
                    if self.role == Role::Candidate {
                        sdk::assert_sometimes(true, S_REELECTION, "");
                    }
                    self.role = Role::Candidate;
                    self.was_candidate = true;
                    self.term += 1;
                    self.voted_for = Some(self.me);
                    self.votes = [self.me].into_iter().collect();
                    self.persist_meta(true).await;
                    self.reset_election();
                    self.next_hb = now + HEARTBEAT;
                    os::log(format!("election timeout -> candidate for term {}", self.term));
                    let (li, lt) = self.last();
                    for i in 0..3 {
                        if i != self.me {
                            self.sock.send(self.addr(i), format!("RV {} {} {li} {lt}", self.term, self.me));
                        }
                    }
                }
            }
        }
    }

    async fn handle(&mut self, p: crate::sim::Packet) {
        let wall = os::wall_clock();
        sdk::assert_sometimes(wall < self.last_wall, S_CLOCK_BACK, "");
        self.last_wall = wall;
        let f: Vec<&str> = p.body.split(' ').collect();
        match f[0] {
            "RV" => {
                let (term, cand, li, lt) = (num(f[1]), num(f[2]) as usize, num(f[3]) as usize, num(f[4]));
                if term > self.term {
                    self.step_down(term).await;
                }
                let (my_li, my_lt) = self.last();
                let up_to_date = lt > my_lt || (lt == my_lt && li >= my_li);
                let grant = term == self.term
                    && (self.voted_for.is_none_or(|v| v == cand) || self.bugs.vote_twice) // BUG(vote_twice)
                    && (up_to_date || self.bugs.log_check); // BUG(log_check)
                if grant {
                    self.voted_for = Some(cand);
                    self.persist_meta(true).await;
                    self.reset_election();
                }
                self.sock.send(p.from, format!("RVR {} {}", self.term, grant as u8));
            }
            "RVR" => {
                let (term, granted) = (num(f[1]), f[2] == "1");
                if term > self.term {
                    self.step_down(term).await;
                } else if self.role == Role::Candidate && term == self.term && granted {
                    self.votes.insert(idx_of(&os_name(p.from.node)));
                    if self.votes.len() >= 2 {
                        self.become_leader().await;
                    }
                }
            }
            "AE" => {
                let (term, leader, prev, prev_term, lcommit, ts) =
                    (num(f[1]), num(f[2]) as usize, num(f[3]) as usize, num(f[4]), num(f[5]) as usize, f[6]);
                if term < self.term {
                    self.sock.send(p.from, format!("AER {} 0 0 {ts}", self.term));
                    return;
                }
                if term > self.term || self.role != Role::Follower {
                    self.step_down(term).await;
                }
                self.leader_hint = Some(leader);
                self.reset_election();
                let ok = prev <= self.log.len() && (prev == 0 || self.log[prev - 1].term == prev_term);
                if !ok {
                    let hint = self.log.len().min(prev.saturating_sub(1));
                    self.sock.send(p.from, format!("AER {} 0 {hint} {ts}", self.term));
                    return;
                }
                let ents: Vec<Entry> = f.get(7).map(|s| s.split(',').filter(|x| !x.is_empty()).map(|x| {
                    let (t, tok) = x.split_once(':').unwrap();
                    Entry { term: num(t), token: tok.to_string() }
                }).collect()).unwrap_or_default();
                let mut truncated = false;
                let mut appended = vec![];
                for (k, e) in ents.iter().enumerate() {
                    let i = prev + k;
                    if i < self.log.len() {
                        if self.log[i].term != e.term {
                            self.log.truncate(i);
                            truncated = true;
                        } else {
                            continue;
                        }
                    }
                    self.log.push(e.clone());
                    appended.push(e.clone());
                }
                if truncated {
                    sdk::assert_sometimes(true, S_REPAIRED, "");
                    os::log(format!("log conflict: truncated and rewrote log to {} entries", self.log.len()));
                    self.persist_log_full().await;
                } else if !appended.is_empty() {
                    for e in &appended {
                        os::append("log", seal(format!("e {} {}", e.term, e.token)));
                    }
                    os::fsync("log").await;
                }
                if self.restarted && !appended.is_empty() {
                    sdk::assert_sometimes(true, S_CAUGHT_UP, "");
                    self.restarted = false;
                }
                let m = prev + ents.len();
                let newc = lcommit.min(m).min(self.log.len());
                if newc > self.commit {
                    self.commit = newc;
                    self.apply_committed();
                }
                self.sock.send(p.from, format!("AER {} 1 {m} {ts}", self.term));
            }
            "AER" => {
                let (term, ok, m, ts) = (num(f[1]), f[2] == "1", num(f[3]) as usize, f[4].parse::<i64>().unwrap_or(0));
                if term > self.term {
                    self.step_down(term).await;
                    return;
                }
                if self.role != Role::Leader || term != self.term {
                    return;
                }
                let i = idx_of(&os_name(p.from.node));
                self.acked_ts[i] = self.acked_ts[i].max(ts);
                if ok {
                    self.match_len[i] = self.match_len[i].max(m);
                    self.next_idx[i] = self.match_len[i] + 1;
                    self.advance_commit();
                } else {
                    self.next_idx[i] = (m + 1).min(self.next_idx[i].saturating_sub(1)).max(1);
                    self.send_append(i);
                }
            }
            // ---- clients
            "W" => {
                let tok = f[1].to_string();
                if self.role != Role::Leader {
                    self.redirect(&p, &tok);
                    return;
                }
                if let Some(i) = self.log.iter().position(|e| e.token == tok) {
                    if i < self.commit {
                        self.sock.send(p.from, format!("OK {tok}"));
                    } else {
                        self.pending.push((i + 1, tok, p.from));
                    }
                    return;
                }
                self.log.push(Entry { term: self.term, token: tok.clone() });
                os::append("log", seal(format!("e {} {tok}", self.term)));
                os::fsync("log").await;
                if self.bugs.no_quorum {
                    // BUG(no_quorum): durable on the leader only
                    self.sock.send(p.from, format!("OK {tok}"));
                } else {
                    let idx = self.log.len();
                    self.pending.push((idx, tok, p.from));
                }
                // replicate right away instead of waiting for the next heartbeat
                for i in 0..3 {
                    if i != self.me {
                        self.send_append(i);
                    }
                }
            }
            "R" | "D" => {
                let rid = f[1];
                if self.role != Role::Leader || !self.committed_in_term() {
                    self.redirect(&p, rid);
                    return;
                }
                if !self.has_lease() {
                    sdk::assert_sometimes(true, S_LEASE_REFUSED, "");
                    self.redirect(&p, rid);
                    return;
                }
                let newest = (0..3).filter(|&i| i != self.me).map(|i| self.acked_ts[i]).max().unwrap();
                sdk::assert_sometimes(self.clock() - newest > (LEASE / 2) as i64, S_LATE_LEASE, "");
                let v = self.value();
                let body = if f[0] == "R" {
                    format!("V {rid} {}", v.len())
                } else {
                    format!("DV {rid} {}", v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(","))
                };
                self.sock.send(p.from, body);
            }
            _ => {}
        }
    }

    fn redirect(&self, p: &crate::sim::Packet, id: &str) {
        let hint = self.leader_hint.filter(|h| *h != self.me).map(|h| NODES[h]).unwrap_or("?");
        self.sock.send(p.from, format!("RD {id} {hint}"));
    }
}

fn num(s: &str) -> u64 {
    s.parse().unwrap_or(0)
}

fn os_name(n: NodeId) -> String {
    crate::sim::with(|w| w.node_name(n))
}

async fn recover(me: usize, bugs: Bugs, ghost: Rc<RefCell<Ghost>>) -> Node {
    let peers: Vec<NodeId> = NODES.iter().map(|n| os::node_id(n)).collect();
    let mut node = Node {
        me,
        peers,
        bugs,
        ghost,
        sock: os::bind(PORT),
        term: 0,
        voted_for: None,
        log: vec![],
        role: Role::Follower,
        commit: 0,
        applied: 0,
        tokens: BTreeSet::new(),
        leader_hint: None,
        election_deadline: 0,
        votes: BTreeSet::new(),
        next_hb: 0,
        next_idx: [1; 3],
        match_len: [0; 3],
        acked_ts: [i64::MIN / 2; 3],
        pending: vec![],
        was_candidate: false,
        restarted: false,
        last_wall: i64::MIN,
    };
    let meta = os::read("meta");
    if let Some(m) = meta.first().and_then(|l| unseal(l)) {
        let f: Vec<&str> = m.split(' ').collect();
        node.term = num(f[1]);
        node.voted_for = f[3].parse().ok();
        node.restarted = true;
    }
    let lines = os::read("log");
    let mut valid = vec![];
    for (i, raw) in lines.iter().enumerate() {
        match unseal(raw) {
            Some(b) => {
                let f: Vec<&str> = b.split(' ').collect();
                node.log.push(Entry { term: num(f[1]), token: f[2].to_string() });
                valid.push(raw.clone());
            }
            None => {
                // a torn tail from a crash mid-append; anything else would be corruption
                os::log(format!("discarding damaged log line {i}: {raw:?}"));
                os::replace("log", valid.clone());
                os::fsync("log").await;
                break;
            }
        }
    }
    if node.restarted {
        os::log(format!("recovered term {} vote {:?} log {}", node.term, node.voted_for, node.log.len()));
    }
    node.reset_election();
    node
}

async fn node_main(me: usize, bugs: Bugs, ghost: Rc<RefCell<Ghost>>) {
    let mut n = recover(me, bugs, ghost).await;
    sdk::setup_complete();
    loop {
        let wake = match n.role {
            Role::Leader => n.next_hb,
            Role::Candidate => n.next_hb.min(n.election_deadline),
            Role::Follower => n.election_deadline,
        };
        let wait = wake.saturating_sub(os::now()).max(1);
        if let Some(p) = n.sock.recv_timeout(wait).await {
            n.handle(p).await;
        }
        n.tick().await;
    }
}

// ------------------------------------------------------------------ workload

#[derive(Default)]
struct Ledger {
    next: u64,
    acked: BTreeSet<String>,
}

/// Send to this client's presumed leader (each client has its own sticky
/// guess), following redirects; None if nobody answered.
async fn call(guess: &mut usize, sock: &os::Socket, id: &str, msg: &str, attempts: u32) -> Option<String> {
    for _ in 0..attempts {
        let target = *guess;
        sock.send(Addr { node: os::node_id(NODES[target]), port: PORT }, msg);
        let deadline = os::now() + 20 * MS;
        let mut next = (target + 1) % 3;
        while os::now() < deadline {
            match sock.recv_timeout(deadline - os::now()).await {
                Some(p) if p.body.split(' ').nth(1) == Some(id) => {
                    if p.body.starts_with("RD ") {
                        sdk::assert_sometimes(true, S_REDIRECT, "");
                        if let Some(h) = p.body.split(' ').nth(2).and_then(|h| NODES.iter().position(|n| *n == h)) {
                            next = h;
                        }
                        os::sleep(2 * MS).await;
                        break;
                    }
                    return Some(p.body);
                }
                Some(_) => continue,
                None => break,
            }
        }
        *guess = next;
    }
    None
}

async fn writes(l: Rc<RefCell<Ledger>>, n: u64) {
    let sock = os::bind_ephemeral();
    let mut guess = sdk::random_choice(3);
    for _ in 0..n {
        let tok = {
            let mut g = l.borrow_mut();
            g.next += 1;
            format!("w{}", g.next)
        };
        if let Some(r) = call(&mut guess, &sock, &tok, &format!("W {tok}"), 8).await {
            if r.starts_with("OK") {
                l.borrow_mut().acked.insert(tok);
            }
        }
        os::sleep(sdk::random_range(0, 3 * MS)).await;
    }
}

async fn reads(l: Rc<RefCell<Ledger>>, n: u64) {
    let sock = os::bind_ephemeral();
    let mut guess = sdk::random_choice(3);
    for k in 0..n {
        let before = l.borrow().acked.len();
        let rid = format!("r{}x{k}", sdk::get_random() % 1_000_000);
        if let Some(r) = call(&mut guess, &sock, &rid, &format!("R {rid}"), 4).await {
            let v: usize = r.split(' ').nth(2).and_then(|x| x.parse().ok()).unwrap_or(0);
            sdk::assert_always(
                v >= before,
                NO_STALE_READ,
                &format!("read returned {v} values but {before} writes were acknowledged before it started"),
            );
        }
        os::sleep(sdk::random_range(MS, 5 * MS)).await;
    }
}

async fn check_all(l: Rc<RefCell<Ledger>>, prop: &'static str) {
    let sock = os::bind_ephemeral();
    let mut guess = sdk::random_choice(3);
    let acked = l.borrow().acked.clone();
    let mut first_missing: Option<Vec<String>> = None;
    for attempt in 0.. {
        let rid = format!("d{}x{attempt}", sdk::get_random() % 1_000_000);
        let Some(r) = call(&mut guess, &sock, &rid, &format!("D {rid}"), 6).await else { continue };
        let have: BTreeSet<&str> = r.split(' ').nth(2).unwrap_or("").split(',').collect();
        let lost: Vec<String> = acked.iter().filter(|t| !have.contains(t.as_str())).cloned().collect();
        if lost.is_empty() {
            sdk::assert_always(true, ACKED_DURABLE, "");
            // writes that looked missing earlier were there all along: that answer was stale
            if let Some(m) = first_missing {
                sdk::assert_always(false, NO_STALE_READ, &format!("final check first read from a deposed leader, missing {m:?}"));
            }
            sdk::assert_reachable(prop, "");
            return;
        }
        if attempt >= 4 {
            sdk::assert_always(false, ACKED_DURABLE, &format!("acknowledged but missing: {lost:?}"));
            return;
        }
        first_missing.get_or_insert(lost);
        // faults are off now; give a deposed leader time to learn it lost its job
        os::sleep(30 * MS).await;
    }
}

fn template(l: Rc<RefCell<Ledger>>) -> Template {
    let mk = |f: fn(Rc<RefCell<Ledger>>) -> crate::sim::LocalFut, l: &Rc<RefCell<Ledger>>| -> MainFn {
        let l = l.clone();
        Rc::new(move || f(l.clone()))
    };
    Template {
        commands: vec![
            cmd("first_wait_for_leader", mk(|_| Box::pin(async move {
                let sock = os::bind_ephemeral();
                let mut guess = 0;
                while call(&mut guess, &sock, "r0", "R r0", 1).await.is_none() {}
            }), &l)),
            cmd("parallel_driver_writes", mk(|l| Box::pin(async move { writes(l, sdk::random_range(3, 8)).await }), &l)),
            cmd("parallel_driver_reads", mk(|l| Box::pin(async move { reads(l, sdk::random_range(3, 8)).await }), &l)),
            cmd("eventually_all_acked_present", mk(|l| Box::pin(check_all(l, R_EVENTUALLY)), &l)),
            cmd("finally_all_acked_present", mk(|l| Box::pin(check_all(l, R_FINALLY)), &l)),
        ],
        driver_time: 600 * MS,
        max_parallel: 4,
    }
}

fn commands() -> Vec<(&'static str, CmdKind)> {
    template(Rc::new(RefCell::new(Ledger::default()))).commands.into_iter().map(|c| (c.name, c.kind)).collect()
}

fn machine(bugs: &BTreeSet<String>, faults: FaultConfig, swarm: bool, buggify: bool) -> MachineSpec {
    let b = Bugs {
        no_quorum: bugs.contains("no_quorum"),
        vote_twice: bugs.contains("vote_twice"),
        log_check: bugs.contains("log_check"),
        lease_clock: bugs.contains("lease_clock"),
    };
    let ghost = Rc::new(RefCell::new(Ghost::default()));
    let mut nodes: Vec<(String, bool, Option<MainFn>)> = NODES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let (b, g) = (b.clone(), ghost.clone());
            let main: MainFn = Rc::new(move || Box::pin(node_main(i, b.clone(), g.clone())));
            (name.to_string(), true, Some(main))
        })
        .collect();
    nodes.push(("workload".into(), false, None));
    let tmpl = Rc::new(template(Rc::new(RefCell::new(Ledger::default()))));
    let composer: MainFn = Rc::new(move || Box::pin(compose(tmpl.clone())));
    MachineSpec { nodes, catalog: catalog(), composer: ("workload".into(), composer), faults, swarm, buggify }
}
