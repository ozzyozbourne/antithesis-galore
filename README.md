# detvisor: an Antithesis-style deterministic testing machine, in miniature

detvisor is a teaching prototype. It shows **what a deterministic hypervisor lets you do as a tester**: the knobs, the dials, and the testing techniques. Every concept maps onto a real Antithesis feature. You can run all of it in a couple of minutes with `./demo.sh`.

It is about 4,900 lines of Rust with no required dependencies. It runs hundreds of timelines per second on one core. It ships three systems under test (`--app`):

* `bank`: a single-node durable bank with a WAL, group commit, compaction and crash recovery.
* `cluster`: a 3-node Raft-style replicated log with leader leases.
* `turso`: the real Turso storage engine (`turso_core`) running on detvisor's simulated disk. Build with `--features turso`.

## What it is, and what it is not

A real deterministic hypervisor runs an unmodified Linux VM. Antithesis' Determinator (a bhyve fork), [bedrock](https://github.com/oss-garage/bedrock) and [dhyve](https://github.com/pgraug/dhyve-src) all work this way. They get determinism from VT-x:

- trap `RDTSC`, `RDRAND` and `CPUID`;
- run the guest on a single core;
- define time as retired instructions;
- inject interrupts at exact instruction counts using the PMU;
- move all I/O through `VMCALL`.

This orb has no `/dev/kvm`, so detvisor emulates the **machine** in software instead:

| Real deterministic hypervisor | detvisor |
|---|---|
| Guest = Linux VM running your docker-compose containers | Guest = async Rust tasks grouped into *nodes* (`bank`, `workload`) |
| Single vCPU, preempted by PMU interrupts at exact instruction counts | Single-threaded scheduler. Every `await` is a preemption point, and the tape picks which thread runs next |
| Time = retired instructions; trapped `RDTSC` returns emulated TSC | Time = virtual nanoseconds. Every scheduling quantum costs 2µs, and idle (`HLT`) jumps to the next timer |
| `RDRAND` / `/dev/urandom` replaced by hypervisor entropy | `sdk::get_random()` / `random_choice()` draw from the tape |
| Virtual NIC + simulated network | `os::bind / send / recv_timeout`: latency, loss, duplication, partitions, clogs |
| Virtual disk | Byte files with a volatile page cache. Only `fsync` makes data durable. A crash keeps a random prefix of unsynced writes, may tear the next one, and bit rot or a lying fsync can be switched on |
| Snapshots of VM memory | Not needed. A timeline is a pure function of the tape, so a *tape prefix* **is** a snapshot (re-executed instead of restored) |

Guest code can't trap here. Determinism holds because the guest only touches the outside world through `sim::os` and `sdk`. `check-determinism` catches you when it doesn't, which is exactly how Antithesis validates its own Determinator.

## The one idea everything rests on: the control tape

Antithesis: *"[the Determinator] replaces all non-deterministic operations ... with deterministic versions controlled by a stream of control signals. For a given set of control signals, the determinator will do exactly the same thing every single time."*

In detvisor that stream is [`Tape`](src/tape.rs). Every decision draws one `u64` from it:
- which thread runs next;
- every packet's latency, and whether it is dropped or duplicated;
- whether a fault fires on this 1ms tick, and which one;
- how much of the page cache survives a crash;
- which test command the composer launches;
- every `random()` the app makes.

The encoding makes **0 the most boring outcome**: the first thread, minimum latency, no fault. That is what makes shrinking work.

Consequences:

* **Replay** = feed the same tape → bit-identical trace.
* **Snapshot at moment t** = the tape prefix consumed up to t.
* **Branch / alternate future** = that prefix followed by fresh entropy.
* **Shrinking** = search for a shorter tape with more zeros that still fails.

## What you write as a user (the customer's side of Antithesis)

1. **Properties, via SDK assertions** ([src/sdk.rs](src/sdk.rs)). There is one property per message, and every message is declared in a catalog (`bank::catalog()`), so "never reached" is detectable.

   | assertion | passes when | fails when |
   |---|---|---|
   | `assert_always` | true every time it is reached | false once, **or never reached** |
   | `assert_always_or_unreachable` | true every time, or never reached | false once |
   | `assert_sometimes` | true at least once in the campaign | never true. You never tested that situation |
   | `assert_reachable` | reached at least once | never reached |
   | `assert_unreachable` | never reached | reached |

   Sometimes/reachable assertions are also **exploration checkpoints**. The guided explorer branches from the moments they fire.

2. **Lifecycle and randomness.**
   - `setup_complete()` means the system is ready. The composer then runs `first_` and switches fault injection on.
   - `get_random()` / `random_choice()` are steerable entropy.
   - `send_event()` writes a structured log line.
   - `stop_faults(d)` is the `ANTITHESIS_STOP_FAULTS` equivalent: it pauses faults and restores crashed nodes.
   - `buggify(site)` is FoundationDB-style internal fault injection.
   - `export_state()` exposes guest state to `inspect`.

3. **A test template for the Test Composer** ([src/composer.rs](src/composer.rs)). These are named commands whose prefix sets their scheduling, so every timeline runs a different random test:

   | prefix | detvisor's bank commands | semantics |
   |---|---|---|
   | `first_` | `first_wait_for_bank` | exactly one, before any driver, no faults |
   | `parallel_driver_` | `parallel_driver_transfers` | concurrent workload, including copies of itself |
   | `serial_driver_` | (supported) | only when no other driver runs |
   | `singleton_driver_` | `singleton_driver_long_transfer_run` | the only driver in that timeline |
   | `anytime_` | `anytime_audit_durability` | any time, alongside anything |
   | `eventually_` | `eventually_recovered_durability` | **kills all drivers, stops all faults**, then checks the system recovers (liveness) |
   | `finally_` | `finally_durability` | only after every started driver completed. Faults stop |

4. **The systems under test**, each with switchable bugs. Every bug is found by a different technique.

   `--app bank` ([src/bank.rs](src/bank.rs)): in-memory balances, a checksummed write-ahead log with group commit, background log compaction into a snapshot, and crash recovery.

   | `--bugs` | the bug | the technique that finds it |
   |---|---|---|
   | `race` | read balances, `await fsync`, write balances: a lost update | scheduler exploring interleavings |
   | `ack` | acknowledges before the WAL is fsynced | crash faults + durability checks |
   | `dup` | no dedup of retried requests | packet loss/duplication + client retries |
   | `compact` | truncates the log before the snapshot is durable (a ~3ms window, deep in the run) | guided exploration from the *"Log compaction started"* checkpoint |
   | `torn` | recovery trusts log lines without verifying checksums | torn-write disk fault |
   | `leak` | iterates a `std::HashMap` (random order per process) | determinism self-check |

   `--app cluster` ([src/cluster.rs](src/cluster.rs)): three nodes running a compact Raft:
   - terms, persisted votes, RequestVote with retries;
   - AppendEntries with log repair;
   - majority commit with a no-op on election;
   - leader leases for reads;
   - clients with sticky, redirect-following connections.

   Global invariants are checked by a test-only "ghost" oracle that all nodes report into: one leader per term, and committed entries never change.

   | `--bugs` | the bug | the technique that finds it |
   |---|---|---|
   | `no_quorum` | acknowledge a write once it is in the leader's own log | partitions/crashes + linearizability check on reads |
   | `vote_twice` | grant a vote without checking `votedFor` | split votes + vote retries; ghost oracle sees two leaders in one term |
   | `log_check` | grant votes without the log up-to-date check | crashes + elections; ghost oracle sees a committed entry overwritten |
   | `lease_clock` | read leases use the wall clock | backward clock skip + a partial partition that outlasts an election. Rare: about 1 in 2,000 random timelines |

   `--app turso` ([src/turso_app.rs](src/turso_app.rs), needs `--features turso`): see [Running real Turso code](#running-real-turso-code).

## The knobs and dials

```
detvisor knobs                      # list every property, fault kind and test command
```

| dial | flag | Antithesis equivalent |
|---|---|---|
| which faults exist and their rates | `--faults default\|none\|network\|aggressive` | fault categories enabled for your tenant |
| opt-in faults | `--enable corrupt,lying_fsync` | (beyond Antithesis' catalog: disk faults) |
| global fault intensity | `--intensity 0.5` | (tuned for you by Antithesis) |
| turn individual fault kinds off | `--disable kill,clock_skip` | ask your FDE to disable a category |
| random subset of faults per timeline | `--swarm` | swarm testing (Groce et al.); Antithesis varies configurations per timeline |
| internal fault injection in the app | `--buggify` | FoundationDB's BUGGIFY (custom faults) |
| how many timelines / where to start | `--runs N --seed S` | test duration |
| exploration strategy | `--strategy random\|guided`, `--horizon-ms` | Antithesis' guidance engine |

Fault kinds ([src/faults.rs](src/faults.rs)), consulted every 1ms tick:
- `latency`: delay variation, loss, duplication
- `partition`: either two groups, or a partial and possibly one-way cut of some of one node's links; Stopped or Slowed; up to 300ms
- `clog`: one node's network Stopped or Slowed
- `pause`: node frozen
- `kill`: crash. Unsynced writes may be lost; restart later
- `stop`: graceful stop
- `thread_pause`: one guest thread frozen
- `clock_skip`: wall clock jumps
- `torn_write`: a crash persists only part of the in-flight write
- `corrupt` (opt-in): bit rot in durable data
- `lying_fsync` (opt-in): fsync reports success and persists nothing

Because faults are decided on every tick, they are decided *late*, so a branch taken at any moment can choose a different future.

## What testing looks like: the techniques, with real output

`./demo.sh` runs all of these. The outputs below are from this orb.

**1. Fault-free testing proves little, and Sometimes assertions tell you so.**
```
$ detvisor run --runs 100 --faults none
   ✓ pass [Always] Acknowledged transfers survive crashes            100 passing / 0 failing timelines
   ✗ FAIL [Sometimes] Server recovered state after a restart         never true: this situation was never tested
   ✗ FAIL [Sometimes] Server deduplicated a retried transfer         never true: this situation was never tested
```

**2. Deterministic simulation + fault injection.** With the correct app, 300 timelines inject about 1,200 faults (144 crashes, 101 partitions, 400 thread pauses, 6 torn writes...) and every property passes. Turn a bug on and it is found:
```
$ detvisor run --bugs race --stop-on-fail     timeline 2: FAIL "Money is conserved" (total is 418 after t2)
$ detvisor run --bugs dup  --stop-on-fail     timeline 1: FAIL "Each transfer is applied at most once" (t4 applied twice)
$ detvisor run --bugs ack  --stop-on-fail     timeline 11: FAIL "Acknowledged transfers survive crashes" (["t28"] missing)
```
Failing tapes are saved in `out/`. A tape is a complete bug report.

**3. Exact replay.** `detvisor replay out/<tape> [--grep FAULT] [--tail N]` re-executes the timeline twice and compares trace hashes (`IDENTICAL`).

**4. Shrinking.** Hypothesis-style: truncate, delete chunks, zero chunks.
```
$ detvisor shrink out/acknowledged-transfers-survive-crashes-1.tape
                         original    shrunk
tape length                  2022       726
faults injected                 3         1
time of failure           202.8ms    74.8ms
  [69.208ms] FAULT node KILL (crash) bank
  [69.208ms]   disk bank: lost 1 unsynced op(s) of 'wal'
  [74.828ms] PROPERTY VIOLATED: "Acknowledged transfers survive crashes" (["t16"] missing)
```
Shrinking works best on shallow bugs. A bug that needs a deep state, such as compaction or the cluster's lease bug, keeps most of its tape.

**5. Guided exploration from interesting moments.** This is Antithesis' main advantage. The compaction bug needs a deep state (the log has 120+ entries, reached in about 10% of timelines) *and* a crash inside a ~3ms window. Random testing pays for the whole run every time. Guided exploration keeps tape prefixes where sometimes-assertions fired, spreads its energy evenly across assertion labels, and runs short branches from them:
```
$ detvisor compare --bugs compact --trials 8 --budget-s 1500
guided: found the bug in 8/8 trials, mean 340s of virtual time
random: found the bug in 4/8 trials, mean 759s when it did
```

**6. Multiverse debugging.** Stop a timeline at any moment (`--at-ms T`, negative = before the failure) or around an event (`--event TEXT`), look inside, and run alternate futures from there:
```
$ detvisor inspect <compact tape> --event KILL --futures 100
event: [422.208ms] hypervisor | FAULT node KILL (crash) bank
--- just BEFORE it ---
  node/bank   UP  disk=[snapshot: 110 durable bytes, 1 unsynced op(s); wal: 17 durable bytes, 0 unsynced op(s)]
  100 alternate futures:  100 x ok                      # the log is already truncated, the snapshot not yet durable
--- just AFTER it ---
  node/bank   DOWN  disk=[snapshot: 110 durable bytes, 0 unsynced op(s); ...]
  100 alternate futures:  100 x FAIL: Recovery finds a snapshot that covers the truncated log
```

**7. Causality analysis.** Rewind to many moments and plot the probability of the bug:
```
$ detvisor causality <compact tape> --points 20 --futures 30
   401.19ms |                                        |   0.0%
   422.30ms |########################################| 100.0%
biggest jump between 401.19ms and 422.30ms. What happened there:
  [419.731ms] sometimes/reachable: "Log compaction started"
  [422.208ms] FAULT node KILL (crash) bank
  [422.208ms]   disk bank: lost 1 unsynced op(s) of 'snapshot'
```

**8. The leaky boat.** One uncontrolled source of nondeterminism and replay is gone:
```
$ detvisor check-determinism --bugs leak
seed 0: DIVERGED at trace line 14
   run 1: ... bal=D=82,B=100,C=100,A=118
   run 2: ... bal=D=82,A=118,B=100,C=100
0/3 seeds reproduced bit-for-bit
```

**9. Liveness via quiescence.** About a third of timelines end in an `eventually_` branch: drivers are killed, faults stop, crashed nodes restart, and the check retries until the system answers. Timelines that let every driver finish run `finally_` instead. The two Reachable properties confirm that both paths were exercised.

**10. Disk faults.**
- Torn writes are survived by the correct bank, because every log line carries a checksum and a torn tail is discarded. With `--bugs torn`, recovery crashes on the half-written line (`index out of bounds`).
- `--enable corrupt` shows that a single node cannot survive bit rot. "Snapshot file is never corrupt" and "WAL is never corrupted before its tail" fail, which is the argument for replication.
- `--enable lying_fsync` loses acknowledged transfers, because no storage system can be correct on such a disk.

**11. A distributed system.** The correct cluster passes 300 faulted timelines (about 2,700 faults: 290 partitions, 380 clogs, 275 crashes...). Every Sometimes situation is exercised: leader changes, log repair, retried elections, expired leases, restarted nodes catching up, a wall clock going backwards. The consensus bugs:
```
$ detvisor run --app cluster --bugs vote_twice --stop-on-fail   timeline 10: FAIL "At most one leader per term" (n2 and n1 both led term 1)
$ detvisor run --app cluster --bugs log_check  --stop-on-fail   timeline  2: FAIL "Committed entries never change" (index 33 committed as w33 earlier, now noop2 on n3)
$ detvisor run --app cluster --bugs no_quorum  --stop-on-fail   timeline  2: FAIL "Reads are never stale" (read returned 13 values but 14 writes were acknowledged)
$ detvisor compare --app cluster --bugs lease_clock --trials 4 --budget-s 1500 --horizon-ms 150
random: found the bug in 1/4 trials
guided: found the bug in 3/4 trials
```
`lease_clock` needs several faults to line up:
- a backward clock skip;
- a partial partition that cuts the leader off from both followers but not from the client;
- an election completing while that partition lasts;
- a client still talking to the deposed leader.

Making this bug findable taught three lessons that Antithesis also stresses:
- the fault catalog needs partial and asymmetric partitions;
- partitions must outlast an election timeout;
- the workload must behave like real clients, with sticky per-client connections.

Before those changes, neither strategy found it.

## Running real Turso code

`--app turso` runs the real Turso storage engine (`turso_core`, pinned in `Cargo.toml` to the Turso commit it was tested against, WAL mode) as the guest on a faultable `db` node. Build it with:

```
cargo build --release --features turso
```

- **IO layer.** Turso does all of its I/O through its `IO`/`File` traits, and detvisor implements those on the simulated disk:
  - reads and writes go to the page cache;
  - `sync` starts an fsync and completes it only after a tape-chosen latency, so a crash can land between a commit's WAL write and its sync;
  - time and randomness come from the machine.
- **Statements** are stepped one IO round at a time with small sleeps in between, so crashes, other tasks and faults interleave with Turso's own I/O.
- **Determinism** holds: `check-determinism --app turso` reproduces 10 of 10 seeds bit-for-bit. Turso starts no threads on this path.
- **The SQL server** handles `INSERT`, `READ`, `CHECK` (`PRAGMA integrity_check`) and `CKPT` (`wal_checkpoint(TRUNCATE)`), with `wal_autocheckpoint = 16`. Before Turso recovers, the server parses the WAL itself to report committed versus torn frames.
- **Properties:**
  - "Committed rows survive crashes"
  - "Rows read back hold the value the workload wrote"
  - "Database passes integrity_check after recovery" and "...while serving"
  - Sometimes: "Recovery found a torn or unsynced WAL tail", "Recovery replayed committed frames from the WAL", "A TRUNCATE checkpoint emptied the WAL", and more.

**Correct mode finds no Turso bug.** About 12,000 faulted timelines passed, including roughly 18,000 crashes and 139 torn writes at 4× intensity.

The app-level bugs are all caught quickly:
```
$ detvisor run --app turso --bugs ack_early --stop-on-fail   timeline 5: FAIL "Committed rows survive crashes" (missing [72])
$ detvisor run --app turso --bugs nosync    --stop-on-fail   timeline 2: FAIL "Database passes integrity_check after recovery" (short read on page 8)
$ detvisor run --app turso --bugs sync_off  --stop-on-fail   timeline 2: FAIL "Database passes integrity_check after recovery" (short read on page 2)
$ detvisor run --app turso --bugs rm_wal    --stop-on-fail   timeline 2: FAIL "Committed rows survive crashes" (missing [77, 78, 79])
```

| `--bugs` | the bug |
|---|---|
| `ack_early` | reply once the commit's sync has been started, before it completes |
| `nosync` | the IO layer's sync reports success without persisting |
| `sync_off` | `PRAGMA synchronous = OFF` |
| `rm_wal` | the startup script deletes the "stale" WAL before opening |

Limits:
- A crash can't land *inside* a single Turso call.
- The simulated disk has no unlink, so removing a file empties it.
- Throughput is about 60 timelines per second.

## Layout

```
src/tape.rs       control tape + PRNG
src/sim.rs        the machine: clock, scheduler, network, disk, nodes, fault injector, guest OS API
src/faults.rs     fault catalog + profiles (the dials)
src/sdk.rs        assertions, randomness, lifecycle, BUGGIFY
src/composer.rs   Test Composer (first_/parallel_driver_/.../eventually_/finally_)
src/apps.rs       registry of systems under test
src/bank.rs       single-node bank: its bugs and test template
src/cluster.rs    3-node Raft cluster: its bugs, ghost oracle and test template
src/turso_app.rs  real Turso under detvisor: IO trait implementation, SQL server, workload (feature `turso`)
src/explore.rs    campaigns, guided exploration, shrinking, causality
src/main.rs       CLI
```

`cargo test --release` checks the core contract: the same tape gives the same timeline, the correct app survives faults, and each bug is found.

## Going from here to a real deterministic hypervisor

Only the bottom layer changes. The SDK, the composer, the explorer and the reports stay the same. Swap `sim.rs` for a VMM:
- VT-x, with exits on `RDTSC`, `RDRAND`, `CPUID` and `HLT`;
- one vCPU pinned to one core;
- the PMU counting retired instructions;
- an overflow just before the target count, then single-stepping with MTF (or EPT-friendly PEBS on Sapphire Rapids);
- `VMCALL` for the SDK;
- copy-on-write memory snapshots instead of re-executing prefixes.

[bedrock](https://github.com/oss-garage/bedrock) (Rust) and [dhyve](https://github.com/pgraug/dhyve-src) (bhyve fork) are the open-source references, and both need a bare-metal Intel host.
